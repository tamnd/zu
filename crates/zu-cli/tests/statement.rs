//! Regenerates `src/statement/features.rs` from the ISO/IEC 39075:2024
//! features artifact and fails on drift, and checks the published
//! conformance statement against the tally it is rendered from. Run with
//! `ZU_UPDATE_STATEMENT=1` to rewrite both.
//!
//! Third table in the tree built this way, after the GQLSTATUS one in
//! zu-common and the Clause 24.5.2 register beside this file, and for
//! the same reason all three are: the codes and the wording are the
//! standard's, so the only honest way to hold them is to copy them
//! mechanically out of the published document and let a test say when
//! the copy has gone stale.
//!
//! The statement itself is checked here rather than left to whoever
//! remembers, because it is the one page in this repository that makes a
//! claim about conformance in so many words. A generated page nobody
//! checks goes stale the first time somebody is in a hurry, and this one
//! going stale would mean the repository claims something no run
//! supports.

use std::path::{Path, PathBuf};

#[derive(Debug, PartialEq, Eq)]
struct Row {
    code: String,
    description: String,
}

fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn repo_root() -> PathBuf {
    manifest_dir()
        .parent()
        .and_then(|p| p.parent())
        .expect("crates/zu-cli is two deep")
        .to_path_buf()
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()))
}

/// Pulls the `<code>` and `<description>` pairs out of the artifact.
///
/// Same reader as the register's, and correct on the same grounds: the
/// document carries a DTD in front of the data, nothing below the DTD
/// uses an entity or an attribute, and the only markup there is the
/// element tags the DTD declares.
fn parse(xml: &str) -> Vec<Row> {
    let body_at = xml.find("]>").expect("the DTD closes") + 2;
    let mut rest = &xml[body_at..];
    let mut rows = Vec::new();
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
        assert!(
            !code.is_empty() && code.len() <= 5,
            "feature code {code:?} is not a feature code"
        );
        rows.push(Row { code, description });
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
        "//! Generated from `crates/zu-common/artifacts/gql-features.xml`, the\n\
         //! ISO/IEC 39075:2024 artifact that lists every optional language\n\
         //! feature the standard defines. Do not edit by hand: run\n\
         //! `ZU_UPDATE_STATEMENT=1 cargo test -p zu-cli --test statement`.\n\
         //!\n\
         //! Codes and descriptions are the standard's, verbatim, with runs of\n\
         //! whitespace folded to one space so a description is one line.\n\
         \n\
         use super::Feature;\n\
         \n",
    );
    out.push_str(
        "/// Every optional feature of ISO/IEC 39075:2024, in code order.\n\
         pub(super) static FEATURES: &[Feature] = &[\n",
    );
    for row in rows {
        out.push_str(&format!(
            "    Feature {{ code: \"{}\", description: \"{}\" }},\n",
            row.code,
            escape(&row.description),
        ));
    }
    out.push_str("];\n");
    out
}

/// Runs the rendered table through rustfmt, so the checked-in file is
/// formatted like every other file in the tree.
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

fn artifact_rows() -> Vec<Row> {
    parse(&read(
        &manifest_dir().join("../zu-common/artifacts/gql-features.xml"),
    ))
}

#[test]
fn generated_table_matches_the_artifact() {
    let rendered = rustfmt(&render(&artifact_rows()));
    let path = manifest_dir().join("src/statement/features.rs");
    if std::env::var_os("ZU_UPDATE_STATEMENT").is_some() {
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
        "src/statement/features.rs is stale; run \
         ZU_UPDATE_STATEMENT=1 cargo test -p zu-cli --test statement"
    );
}

/// The count the statement divides by. A denominator that came from
/// counting rows by hand is one that goes wrong the first time the
/// standard is amended.
#[test]
fn the_artifact_holds_the_count_the_statement_quotes() {
    assert_eq!(artifact_rows().len(), 228, "optional features");
}

/// The published statement is the render of zu's own tally, and stays
/// that way.
#[test]
fn the_published_statement_matches_the_tally() {
    let tally = repo_root().join("docs/conformance/zu.json");
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_zu"))
        .args(["conformance", "--statement"])
        .arg(&tally)
        .output()
        .expect("run zu conformance --statement");
    assert!(
        out.status.success(),
        "zu conformance --statement failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let rendered = String::from_utf8(out.stdout).expect("the statement is utf-8");

    let path = repo_root().join("docs/gql-conformance-statement.md");
    if std::env::var_os("ZU_UPDATE_STATEMENT").is_some() {
        std::fs::write(&path, &rendered).expect("write the statement");
        return;
    }
    let on_disk = read(&path).replace("\r\n", "\n");
    assert_eq!(
        on_disk, rendered,
        "docs/gql-conformance-statement.md is stale; rerun with ZU_UPDATE_STATEMENT=1"
    );
}

/// Every feature the statement claims is a feature the standard defines.
///
/// The claim list comes from a corpus in another repository, so this is
/// the seam where a code that engine and harness agree on but the
/// standard has never heard of would arrive. It would otherwise be
/// published as a claim with a blank beside it.
#[test]
fn every_claimed_feature_is_one_the_standard_defines() {
    let tally = read(&repo_root().join("docs/conformance/zu.json"));
    let at = tally
        .find("\"features_claimed\": [")
        .expect("the tally lists the features claimed");
    let list = &tally[at..];
    let end = list.find(']').expect("the list closes");
    let known: Vec<String> = artifact_rows().into_iter().map(|r| r.code).collect();
    let mut claimed = 0;
    for code in list[..end].split('"').filter(|s| {
        s.len() >= 3
            && s.chars().next().is_some_and(|c| c.is_ascii_uppercase())
            && s.chars().last().is_some_and(|c| c.is_ascii_digit())
    }) {
        claimed += 1;
        assert!(
            known.iter().any(|k| k == code),
            "the tally claims {code}, which the artifact does not define"
        );
    }
    assert!(claimed > 0, "the tally claims no features at all");
}
