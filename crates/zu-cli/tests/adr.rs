//! The `docs/adr/` index against the records it indexes, and the
//! README's client table against the repositories ADR 0005 splits the
//! project into.
//!
//! Neither is the CLI's business, but `conformance_toml.rs` next door
//! already checks a repo-root file from here and one home for that kind
//! of test beats two. What both checks have in common is the failure
//! they prevent: a hand-maintained index that drifts from what it
//! indexes is worse than no index, because a reader trusts it.

use std::collections::BTreeSet;
use std::path::PathBuf;

/// The repo root, two levels up from `crates/zu-cli`.
fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("crates/zu-cli is two deep")
        .to_path_buf()
}

fn read(rel: &str) -> String {
    let path = repo_root().join(rel);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// Every `NNNN-*.md` in `docs/adr/`, by file name, in number order.
fn record_files() -> BTreeSet<String> {
    let dir = repo_root().join("docs/adr");
    std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("read {}: {e}", dir.display()))
        .map(|entry| entry.expect("dir entry").file_name())
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| name != "README.md" && name.ends_with(".md"))
        .collect()
}

/// The file names the index links to, in the order it lists them.
fn indexed() -> Vec<String> {
    read("docs/adr/README.md")
        .lines()
        .filter_map(|line| {
            let rest = line.strip_prefix("| [")?;
            let (_, rest) = rest.split_once("](")?;
            let (target, _) = rest.split_once(')')?;
            Some(target.to_owned())
        })
        .collect()
}

/// The number a record's file name starts with.
fn number_of(name: &str) -> u32 {
    name.split('-')
        .next()
        .and_then(|n| n.parse().ok())
        .unwrap_or_else(|| panic!("{name} does not start with a number"))
}

#[test]
fn the_index_lists_every_record_and_nothing_else() {
    let files = record_files();
    let listed = indexed();
    assert!(!listed.is_empty(), "the index links to nothing at all");
    for target in &listed {
        assert!(
            files.contains(target),
            "the index links to {target}, which is not in docs/adr/"
        );
    }
    for file in &files {
        assert!(
            listed.contains(file),
            "{file} is a record the index does not list"
        );
    }
    assert_eq!(listed.len(), files.len(), "the index lists one twice");
}

#[test]
fn numbers_are_allocated_in_order_and_never_reused() {
    let numbers: Vec<u32> = record_files().iter().map(|f| number_of(f)).collect();
    for (i, n) in numbers.iter().enumerate() {
        assert_eq!(
            *n as usize,
            i + 1,
            "records are numbered from 1 with no gaps, and this one is {n} at position {}",
            i + 1
        );
    }
    // The index is in the same order, so a reader scanning it reads the
    // decisions in the order they were taken.
    let listed: Vec<u32> = indexed().iter().map(|f| number_of(f)).collect();
    assert_eq!(listed, numbers, "the index is out of order");
}

#[test]
fn every_record_says_where_it_stands() {
    for file in record_files() {
        let body = read(&format!("docs/adr/{file}"));
        let status = body
            .lines()
            .find_map(|line| line.strip_prefix("Status: "))
            .unwrap_or_else(|| panic!("{file} has no Status line"));
        let kind = status
            .split(&[',', ' '][..])
            .next()
            .expect("split always yields one");
        assert!(
            ["proposed", "accepted", "superseded"].contains(&kind),
            "{file} has status {kind:?}, which is not one of the three"
        );
        // A superseded record has to name what replaced it, or the log
        // has a dead end in it.
        if kind == "superseded" {
            assert!(
                status.contains("ADR "),
                "{file} is superseded and does not name the record that replaced it"
            );
        }
        assert!(
            body.starts_with(&format!("# {:04}. ", number_of(&file))),
            "{file} does not open with its own number"
        );
        for heading in [
            "## Context",
            "## Decision",
            "## Consequences",
            "## Rejected",
        ] {
            assert!(body.contains(heading), "{file} has no {heading} section");
        }
    }
}

#[test]
fn the_readme_lists_every_repository_the_split_creates() {
    // ADR 0005 splits the project into nine. This one is the ninth, and
    // the other eight are what a reader landing here has to be able to
    // find, since a client is not a directory in this tree.
    let readme = read("README.md");
    for repo in [
        "zu-c",
        "zu-python",
        "zu-node",
        "zu-go",
        "zu-java",
        "zu-dotnet",
        "zu-kit",
        "zu-web",
    ] {
        let link = format!("https://github.com/tamnd/{repo}");
        assert!(
            readme.contains(&link),
            "the README does not link to {repo}, which ADR 0005 says is its own repository"
        );
    }
    // And it says where an engine bug goes, because a client tracker
    // filling up with them is the failure ADR 0005 names.
    assert!(
        readme.contains("reproduces through the `zu` CLI"),
        "the README does not say where a bug found through a client belongs"
    );
}
