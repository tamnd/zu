//! Regenerates `src/impdef/generated.rs` from the two ISO/IEC 39075:2024
//! artifacts that list what the standard leaves to the implementation,
//! and fails on drift. Run with `ZU_UPDATE_IMPDEF=1` to rewrite the file
//! after an artifact changes.
//!
//! Same arrangement as the GQLSTATUS table one crate over, and for the
//! same reason: the codes and the wording of the items are the
//! standard's and not ours, so the only honest way to hold them is to
//! copy them mechanically out of the published document and let a test
//! say when the copy has gone stale.
//!
//! The artifacts live in `crates/zu-common/artifacts` with the rest of
//! the vendored ISO material and their hashes in the SHA256SUMS beside
//! them, which is why this test reaches across a crate boundary to read
//! them. One directory holding every artifact is worth more than every
//! artifact sitting next to the crate that happens to read it: the check
//! that the bytes are the published bytes is one command either way, and
//! there is one place to look.
//!
//! The table itself is here rather than in zu-common because nothing in
//! the engine reads it. It is the answer sheet the report is rendered
//! from, so it belongs to the tool that renders the report and its
//! twenty five kilobytes stay out of every binary that links the
//! library.

use std::path::{Path, PathBuf};

#[derive(Debug, PartialEq, Eq)]
struct Row {
    code: String,
    kind: &'static str,
    description: String,
}

fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// The vendored artifacts, which live with zu-common's.
fn artifact(name: &str) -> PathBuf {
    manifest_dir().join("../zu-common/artifacts").join(name)
}

/// Pulls the `<code>` and `<description>` pairs out of one artifact.
///
/// Both documents carry a DTD in front of the data that declares
/// elements by those names, so the scan starts after the DTD closes.
/// Neither document uses an XML entity or an attribute anywhere in its
/// body, which is what lets a reader this small be correct: the only
/// markup below the DTD is the four element tags the DTD declares.
fn parse(xml: &str, kind: &'static str) -> Vec<Row> {
    let body_at = xml.find("]>").expect("the DTD closes") + 2;
    let body = &xml[body_at..];
    let mut rows = Vec::new();
    let mut rest = body;
    while let Some(at) = rest.find("<code>") {
        let after = &rest[at + "<code>".len()..];
        let end = after.find("</code>").expect("a code element closes");
        let code = after[..end].trim().to_string();
        rest = &after[end..];
        let at = rest
            .find("<description>")
            .expect("a code carries a description");
        let after = &rest[at + "<description>".len()..];
        let end = after
            .find("</description>")
            .expect("a description element closes");
        let description = after[..end]
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        rest = &after[end..];
        assert_eq!(code.len(), 5, "item code {code:?} is not five characters");
        rows.push(Row {
            code,
            kind,
            description,
        });
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
        "//! Generated from `crates/zu-common/artifacts/gql-implementation-defined.xml`\n\
         //! and `gql-implementation-dependent.xml`, the two ISO/IEC\n\
         //! 39075:2024 artifacts that list what the standard leaves to the\n\
         //! implementation. Do not edit by hand: run\n\
         //! `ZU_UPDATE_IMPDEF=1 cargo test -p zu-cli --test impdef_table`.\n\
         //!\n\
         //! Codes and descriptions are the standard's, verbatim, with runs\n\
         //! of whitespace folded to one space so a description is one line.\n\
         \n\
         use super::{Item, Kind};\n\
         \n",
    );
    out.push_str(
        "/// Every item the standard leaves open, in code order, the\n\
         /// implementation-defined ones first.\n\
         pub(super) static ITEMS: &[Item] = &[\n",
    );
    for row in rows {
        out.push_str(&format!(
            "    Item {{ code: \"{}\", kind: Kind::{}, description: \"{}\" }},\n",
            row.code,
            row.kind,
            escape(&row.description),
        ));
    }
    out.push_str("];\n");
    out
}

/// Runs the rendered table through rustfmt so the checked-in file is
/// formatted like every other file in the tree and `cargo fmt --check`
/// has nothing to say about it.
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

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()))
}

fn artifact_rows() -> Vec<Row> {
    let mut rows = parse(
        &read(&artifact("gql-implementation-defined.xml")),
        "Defined",
    );
    rows.extend(parse(
        &read(&artifact("gql-implementation-dependent.xml")),
        "Dependent",
    ));
    rows
}

#[test]
fn generated_table_matches_the_artifacts() {
    let rows = artifact_rows();
    let rendered = rustfmt(&render(&rows));

    let path = manifest_dir().join("src/impdef/generated.rs");
    if std::env::var_os("ZU_UPDATE_IMPDEF").is_some() {
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(&path, &rendered).expect("write generated table");
        return;
    }
    // Checkout on Windows rewrites the line endings, so compare the
    // content rather than the bytes.
    let have = read(&path).replace("\r\n", "\n");
    assert_eq!(
        have.trim_end(),
        rendered.trim_end(),
        "src/impdef/generated.rs is stale; run \
         ZU_UPDATE_IMPDEF=1 cargo test -p zu-cli --test impdef_table"
    );
}

/// The published register, which is the render of the answers next to
/// the items and is what a reader of the conformance statement is
/// pointed at. Checked in and drift tested for the same reason
/// `conformance.toml` is: a generated file nothing checks goes stale the
/// first time somebody is in a hurry, and this one is a published claim
/// about conformance rather than a convenience.
#[test]
fn the_published_register_matches_the_answers() {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_zu"))
        .args(["conformance", "--implementation-defined"])
        .output()
        .expect("run zu conformance --implementation-defined");
    assert!(
        out.status.success(),
        "zu conformance --implementation-defined failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let rendered = String::from_utf8(out.stdout).expect("the register is utf-8");

    let path = manifest_dir()
        .parent()
        .and_then(|p| p.parent())
        .expect("crates/zu-cli is two deep")
        .join("docs/gql-implementation-defined.md");
    if std::env::var_os("ZU_UPDATE_IMPDEF").is_some() {
        std::fs::write(&path, &rendered).expect("write the register");
        return;
    }
    let on_disk = read(&path).replace("\r\n", "\n");
    assert_eq!(
        on_disk, rendered,
        "docs/gql-implementation-defined.md is stale; rerun with ZU_UPDATE_IMPDEF=1"
    );
}

/// The two counts the conformance statement quotes. A number in a
/// published claim that came from counting rows by hand is a number that
/// goes wrong the first time the standard is amended.
#[test]
fn the_artifacts_hold_the_counts_the_statement_quotes() {
    let rows = artifact_rows();
    assert_eq!(
        rows.iter().filter(|r| r.kind == "Defined").count(),
        117,
        "implementation-defined items"
    );
    assert_eq!(
        rows.iter().filter(|r| r.kind == "Dependent").count(),
        20,
        "implementation-dependent items"
    );
}
