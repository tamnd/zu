//! Regenerates `docs/gql-performance.md` from the measurement sets
//! checked in under `docs/performance/` and fails on drift. Run with
//! `ZU_UPDATE_CONFORMANCE=1` to rewrite it.
//!
//! The same split as the conformance scoreboard next door: the numbers
//! come from a harness run against a real engine, which this test suite
//! has no business starting, and this only checks that the page in the
//! repository is the one those numbers produce. What is different here
//! is that the numbers themselves are timings, so nothing in this file
//! compares one run against another. A page that no longer matches its
//! inputs is a bug; a timing that moved is a Tuesday.

use std::path::PathBuf;
use std::process::Command;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("crates/zu-cli is two deep")
        .to_path_buf()
}

/// Every measurement set in `docs/performance`, sorted by filename, so
/// the column order is a property of the repository and not of how a
/// filesystem happened to enumerate a directory.
fn sets() -> Vec<PathBuf> {
    let dir = repo_root().join("docs").join("performance");
    let mut paths: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("read {}: {e}", dir.display()))
        .map(|e| e.expect("dir entry").path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .collect();
    paths.sort();
    assert!(!paths.is_empty(), "no measurements in {}", dir.display());
    paths
}

fn rendered() -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_zu"))
        .args(["conformance", "--matrix"])
        .args(sets())
        .output()
        .expect("run zu conformance --matrix");
    assert!(
        out.status.success(),
        "zu conformance --matrix failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).expect("the matrix is utf-8")
}

#[test]
fn the_matrix_matches_the_measurements() {
    let page = rendered();
    let path = repo_root().join("docs").join("gql-performance.md");
    if std::env::var_os("ZU_UPDATE_CONFORMANCE").is_some() {
        std::fs::write(&path, &page).expect("write docs/gql-performance.md");
        return;
    }
    let on_disk = std::fs::read_to_string(&path)
        .unwrap_or_default()
        .replace("\r\n", "\n");
    assert_eq!(
        on_disk, page,
        "docs/gql-performance.md is stale; rerun with ZU_UPDATE_CONFORMANCE=1"
    );
}

#[test]
fn every_set_is_readable_and_names_an_engine() {
    // A set that fails to load renders as "unknown" in the page and
    // nowhere else, which is a way to publish a broken column and never
    // find out.
    for p in sets() {
        let text = std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("read {p:?}: {e}"));
        let stem = p.file_stem().expect("stem").to_string_lossy().to_string();
        assert!(
            text.contains(&format!("\"engine\": \"{stem}\"")),
            "{p:?} is named after {stem} and does not declare that engine"
        );
        for field in ["version", "taken", "host", "round_trip_ns", "cases", "loads"] {
            assert!(
                text.contains(&format!("\"{field}\"")),
                "{p:?} has no {field}"
            );
        }
    }
}

#[test]
fn every_column_carries_a_timed_case_and_a_load() {
    // An engine that ran no performance case at all would render as a
    // column of dashes, which reads on the page like an engine that was
    // slow rather than one that was never measured.
    for p in sets() {
        let text = std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("read {p:?}: {e}"));
        assert!(
            text.contains("\"p50_ns\""),
            "{p:?} has no timed case in it"
        );
        assert!(
            text.contains("\"engine_wall_ns\""),
            "{p:?} has no ingest in it"
        );
    }
}

#[test]
fn the_page_says_whether_the_columns_were_taken_together() {
    // The one sentence a reader of a comparison needs before any of the
    // numbers, and the one thing they cannot work out for themselves.
    let page = rendered();
    assert!(
        page.contains("were taken together") || page.contains("were not all taken together"),
        "the page makes no claim either way about when its columns were measured"
    );
}
