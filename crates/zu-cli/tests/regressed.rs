//! `zu conformance --regressed` is the gate the per-PR conformance job
//! runs, so its edge cases are worth pinning from outside the binary.
//!
//! The one that matters most is the subset case. The PR job runs two
//! kinds out of five to keep the wall clock down, and a gate that read
//! the three it did not run as three kinds that fell to zero would fail
//! every PR, which within a week means somebody deletes the job.

use std::path::{Path, PathBuf};
use std::process::Command;

/// A directory of this test's own. Named after the test rather than
/// only the process, because the two tests here run on separate threads
/// of one process and a shared directory means each one overwrites the
/// other's baseline.
fn tmp(test: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("zu-regressed-{}-{test}", std::process::id()));
    std::fs::create_dir_all(&d).expect("mkdir");
    d
}

/// A report with the counts the test wants, trimmed to the fields the
/// tally reads.
fn report(kinds: &str, cases: &str) -> String {
    format!(
        r#"{{"tool":"t","generated":"2026-08-13T10:00:00+07:00",
        "engine":{{"adapter":"zu","version":"zu 0.0.1"}},
        "host":{{"os":"linux","arch":"x86_64","cpu_model":"c"}},
        "run":{{"selector":"s"}},
        "totals":{{"cases":0,"pass":0,"fail":0,"skip":0,"error":0,"by_kind":{{{kinds}}}}},
        "coverage":{{"features":{{}}}},
        "cases":[{cases}]}}"#
    )
}

fn kind(name: &str, cases: u64, pass: u64, fail: u64, error: u64) -> String {
    format!(r#""{name}":{{"cases":{cases},"pass":{pass},"fail":{fail},"skip":0,"error":{error}}}"#)
}

/// Writes a report, tallies it, and returns the tally's path.
fn baseline(dir: &Path, name: &str, body: &str) -> PathBuf {
    let src = dir.join(format!("{name}-report.json"));
    std::fs::write(&src, body).expect("write");
    let out = Command::new(env!("CARGO_BIN_EXE_zu"))
        .args(["conformance", "--tally"])
        .arg(&src)
        .output()
        .expect("tally");
    assert!(out.status.success(), "tally failed");
    let path = dir.join(format!("{name}.json"));
    std::fs::write(&path, out.stdout).expect("write tally");
    path
}

fn run(report_path: &Path, baseline_path: &Path) -> (bool, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_zu"))
        .args(["conformance", "--regressed"])
        .arg(report_path)
        .arg(baseline_path)
        .output()
        .expect("regressed");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
    )
}

#[test]
fn a_fall_fails_a_rise_passes_and_a_kind_that_did_not_run_is_named() {
    let dir = tmp("gate");
    let full = format!(
        "{},{},{}",
        kind("mandatory", 10, 6, 4, 0),
        kind("condition", 10, 5, 4, 1),
        kind("optional", 10, 3, 7, 0)
    );
    let base = baseline(&dir, "base", &report(&full, ""));

    // Same numbers: green, and no noise.
    let same = dir.join("same.json");
    std::fs::write(&same, report(&full, "")).expect("write");
    let (ok, out, _) = run(&same, &base);
    assert!(ok, "an unchanged run was called a regression");
    assert!(out.contains("nothing regressed"), "{out}");

    // One kind passes fewer: red, and it says which and by how much.
    let worse = format!(
        "{},{},{}",
        kind("mandatory", 10, 5, 5, 0),
        kind("condition", 10, 5, 4, 1),
        kind("optional", 10, 3, 7, 0)
    );
    let bad = dir.join("worse.json");
    std::fs::write(&bad, report(&worse, "")).expect("write");
    let (ok, _, err) = run(&bad, &base);
    assert!(!ok, "a fall was accepted");
    assert!(err.contains("mandatory: 5 passing, was 6"), "{err}");

    // One kind passes more: green, and it says so rather than staying
    // quiet, because an improvement nobody notices is an improvement
    // that never reaches the scoreboard.
    let better = format!(
        "{},{},{}",
        kind("mandatory", 10, 8, 2, 0),
        kind("condition", 10, 5, 4, 1),
        kind("optional", 10, 3, 7, 0)
    );
    let good = dir.join("better.json");
    std::fs::write(&good, report(&better, "")).expect("write");
    let (ok, out, _) = run(&good, &base);
    assert!(ok, "a rise was called a regression");
    assert!(out.contains("up, mandatory: 8 passing, was 6"), "{out}");
    assert!(out.contains("regenerate the tally"), "{out}");

    // A subset run, which is what the PR job does. The three kinds it
    // did not run must not count as three falls, and the gate has to
    // say out loud that it did not look at them.
    let subset = format!(
        "{},{}",
        kind("mandatory", 10, 6, 4, 0),
        kind("condition", 10, 5, 4, 1)
    );
    let part = dir.join("subset.json");
    std::fs::write(&part, report(&subset, "")).expect("write");
    let (ok, out, err) = run(&part, &base);
    assert!(ok, "a subset run was called a regression: {err}");
    assert!(
        out.contains("not compared"),
        "the gate did not say what it skipped: {out}"
    );
    assert!(out.contains("optional"), "{out}");

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_case_that_never_reached_a_verdict_may_not_become_two() {
    // Errors are the one count that may go down and hold but not rise.
    // A case that errored did not measure the engine, and a change that
    // turns a verdict into an error has made the report say less while
    // leaving the pass count alone, which no other check here would see.
    let dir = tmp("errors");
    let base = baseline(&dir, "base", &report(&kind("condition", 10, 5, 4, 1), ""));

    let more_errors = dir.join("errors.json");
    std::fs::write(&more_errors, report(&kind("condition", 10, 5, 2, 3), "")).expect("write");
    let (ok, _, err) = run(&more_errors, &base);
    assert!(!ok, "a rise in unmeasured cases was accepted");
    assert!(err.contains("never reached a verdict"), "{err}");

    let fewer_errors = dir.join("fixed.json");
    std::fs::write(&fewer_errors, report(&kind("condition", 10, 5, 5, 0), "")).expect("write");
    let (ok, _, err) = run(&fewer_errors, &base);
    assert!(ok, "fixing an error was called a regression: {err}");

    std::fs::remove_dir_all(&dir).ok();
}
