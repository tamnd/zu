//! Regenerates `src/keywords/generated.rs` from the published GQL BNF
//! and fails on drift, the same way the GQLSTATUS and Unicode
//! generators work. Run with `ZU_UPDATE_KEYWORDS=1 cargo test -p
//! zu-common --test keyword_table` after the artifact changes.
//!
//! The artifact is checked in at `artifacts/gql-bnf.xml` with its
//! SHA-256 next to it, and `make check-artifacts` verifies the bytes.
//!
//! Three productions matter here. `<reserved word>` is the list a
//! regular identifier may not be spelled as, `<pre-reserved word>` is
//! an alternative inside it holding the words ISO has taken for a later
//! edition, and `<non-reserved word>` is every other word the grammar
//! writes, which an identifier may be spelled as. The generator keeps
//! the three apart rather than flattening them, because the difference
//! between them is exactly what the engine has to decide about.

use std::path::PathBuf;

fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn artifact() -> String {
    std::fs::read_to_string(manifest_dir().join("artifacts/gql-bnf.xml")).expect("the BNF artifact")
}

/// The `<kw>` texts of one `<BNFdef>`, without the ones that belong to
/// a production it refers to.
///
/// The artifact writes a definition as `<BNFdef name="..."> ... </BNFdef>`
/// and nests no definition inside another, so the slice between the two
/// tags is the whole of one production and nothing else.
fn keywords(xml: &str, production: &str) -> Vec<String> {
    let open = format!("<BNFdef name=\"{production}\"");
    let at = xml.find(&open).unwrap_or_else(|| panic!("<{production}>"));
    let end = xml[at..].find("</BNFdef>").expect("a closing tag") + at;
    let body = &xml[at..end];

    let mut out = Vec::new();
    let mut rest = body;
    while let Some(start) = rest.find("<kw>") {
        let after = &rest[start + 4..];
        let stop = after.find("</kw>").expect("a closing kw tag");
        out.push(after[..stop].trim().to_string());
        rest = &after[stop..];
    }
    out.sort();
    out.dedup();
    out
}

fn render(reserved: &[String], pre: &[String], non: &[String]) -> String {
    let mut out = String::new();
    out.push_str(
        "//! Generated from `artifacts/gql-bnf.xml`, the ISO/IEC\n\
         //! 39075:2024 grammar artifact. Do not edit by hand: run\n\
         //! `ZU_UPDATE_KEYWORDS=1 cargo test -p zu-common --test keyword_table`.\n\
         //!\n\
         //! Every list is sorted, so the lookups are a binary search.\n\
         \n",
    );
    let mut table = |name: &str, doc: &str, words: &[String]| {
        out.push_str(doc);
        out.push_str(&format!(
            "#[rustfmt::skip]\npub(super) static {name}: &[&str] = &[\n"
        ));
        for (i, word) in words.iter().enumerate() {
            out.push_str(if i.is_multiple_of(6) { "    " } else { " " });
            out.push_str(&format!("\"{word}\","));
            if (i + 1).is_multiple_of(6) {
                out.push('\n');
            }
        }
        if !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str("];\n\n");
    };
    table(
        "RESERVED",
        "/// The words `<reserved word>` names directly, which a regular\n\
         /// identifier may not be spelled as (ISO 21.3).\n",
        reserved,
    );
    table(
        "PRE_RESERVED",
        "/// The words `<pre-reserved word>` names, an alternative inside\n\
         /// `<reserved word>` holding what ISO has taken for a later\n\
         /// edition and given no meaning to yet.\n",
        pre,
    );
    table(
        "NON_RESERVED",
        "/// The words `<non-reserved word>` names: spelled like keywords,\n\
         /// admitted as identifiers.\n",
        non,
    );
    out
}

/// Runs the rendered table through rustfmt so the checked-in file is
/// formatted like every other file in the tree, the same reason the
/// other two generators do it.
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
    let xml = artifact();
    let rendered = rustfmt(&render(
        &keywords(&xml, "reserved word"),
        &keywords(&xml, "pre-reserved word"),
        &keywords(&xml, "non-reserved word"),
    ));

    let path = manifest_dir().join("src/keywords/generated.rs");
    if std::env::var_os("ZU_UPDATE_KEYWORDS").is_some() {
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(&path, &rendered).expect("write generated table");
        return;
    }
    let on_disk = std::fs::read_to_string(&path)
        .unwrap_or_default()
        .replace("\r\n", "\n");
    assert_eq!(
        on_disk, rendered,
        "src/keywords/generated.rs is stale; rerun with ZU_UPDATE_KEYWORDS=1"
    );
}

/// The counts the engine's behaviour is settled against, so a swapped
/// artifact that still parses does not quietly change what a name is.
#[test]
fn the_artifact_has_the_shape_the_lexer_assumes() {
    let xml = artifact();
    let reserved = keywords(&xml, "reserved word");
    let pre = keywords(&xml, "pre-reserved word");
    let non = keywords(&xml, "non-reserved word");
    assert_eq!(reserved.len(), 222, "the words reserved outright");
    assert_eq!(pre.len(), 40, "the words reserved for a later edition");
    assert_eq!(non.len(), 48, "the words that are still names");
    // The three lists do not overlap. `<reserved word>` refers to
    // `<pre-reserved word>` rather than repeating it, which is what
    // makes reading the two separately the right thing to do.
    for word in &pre {
        assert!(!reserved.contains(word), "{word} is in both lists");
    }
    for word in &non {
        assert!(!reserved.contains(word), "{word} is reserved and not");
        assert!(!pre.contains(word), "{word} is pre-reserved and not");
    }
    assert!(reserved.contains(&"MATCH".to_string()));
    assert!(pre.contains(&"UNIT".to_string()));
}
