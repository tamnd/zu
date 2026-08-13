//! Regenerates `docs/gql-conformance.md` from the tallies checked in
//! under `docs/conformance/` and fails on drift. Run with
//! `ZU_UPDATE_CONFORMANCE=1` to rewrite it.
//!
//! The tallies themselves are not regenerated here. They come from a
//! full harness run against a real engine, which this test suite has no
//! business starting, so a tally arrives by hand or from the nightly
//! job and this test only checks that the page in the repository is the
//! one those tallies produce. That split is the point: the numbers are
//! measured somewhere accountable and the page is derived, so nobody
//! can improve the page without improving something first.

use std::path::PathBuf;
use std::process::Command;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("crates/zu-cli is two deep")
        .to_path_buf()
}

/// Every tally in `docs/conformance`, sorted by filename.
///
/// Sorted rather than glob order so the page does not depend on how a
/// shell or a filesystem happened to enumerate the directory. It is a
/// generated file compared byte for byte, and a column order that moves
/// on its own would fail the drift check on a machine that did nothing
/// wrong.
fn tallies() -> Vec<PathBuf> {
    let dir = repo_root().join("docs").join("conformance");
    let mut paths: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("read {}: {e}", dir.display()))
        .map(|e| e.expect("dir entry").path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .collect();
    paths.sort();
    assert!(!paths.is_empty(), "no tallies in {}", dir.display());
    paths
}

fn rendered() -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_zu"))
        .args(["conformance", "--scoreboard"])
        .args(tallies())
        .output()
        .expect("run zu conformance --scoreboard");
    assert!(
        out.status.success(),
        "zu conformance --scoreboard failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).expect("the scoreboard is utf-8")
}

#[test]
fn the_scoreboard_matches_the_tallies() {
    let page = rendered();
    let path = repo_root().join("docs").join("gql-conformance.md");
    if std::env::var_os("ZU_UPDATE_CONFORMANCE").is_some() {
        std::fs::write(&path, &page).expect("write docs/gql-conformance.md");
        return;
    }
    let on_disk = std::fs::read_to_string(&path)
        .unwrap_or_default()
        .replace("\r\n", "\n");
    assert_eq!(
        on_disk, page,
        "docs/gql-conformance.md is stale; rerun with ZU_UPDATE_CONFORMANCE=1"
    );
}

#[test]
fn every_tally_is_readable_and_names_an_engine() {
    // A tally that fails to load renders as "unknown" in the page and
    // nowhere else, which is a way to publish a broken column and not
    // find out. Fail here instead.
    for p in tallies() {
        let text = std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("read {p:?}: {e}"));
        let stem = p.file_stem().expect("stem").to_string_lossy().to_string();
        assert!(
            text.contains(&format!("\"engine\": \"{stem}\"")),
            "{p:?} is named after {stem} and does not declare that engine"
        );
        for field in ["version", "taken", "host", "cases", "pass", "by_kind"] {
            assert!(
                text.contains(&format!("\"{field}\"")),
                "{p:?} has no {field}"
            );
        }
    }
}

#[test]
fn a_tally_of_the_reports_zu_can_produce_round_trips_through_the_page() {
    // The narrow thing this catches: a tally whose numbers do not add
    // up, which would put a percentage on the page that no arithmetic
    // supports. pass + fail + skip + error has to be the case count.
    for p in tallies() {
        let text = std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("read {p:?}: {e}"));
        let n = |key: &str| -> u64 {
            let at = text
                .find(&format!("\"{key}\": "))
                .unwrap_or_else(|| panic!("{p:?} has no {key}"))
                + key.len()
                + 4;
            text[at..]
                .chars()
                .take_while(char::is_ascii_digit)
                .collect::<String>()
                .parse()
                .unwrap_or_else(|e| panic!("{p:?} {key} is not a number: {e}"))
        };
        assert_eq!(
            n("cases"),
            n("pass") + n("fail") + n("skip") + n("error"),
            "{p:?} does not add up"
        );
    }
}
