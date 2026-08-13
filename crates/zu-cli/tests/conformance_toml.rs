//! Regenerates `conformance.toml` at the repo root from `zu conformance
//! --declare` and fails on drift. Run with `ZU_UPDATE_CONFORMANCE=1` to
//! rewrite it after the declaration changes.
//!
//! Same arrangement as the GQLSTATUS table in zu-common, and for the
//! same reason: a generated file that nothing checks is a file that goes
//! stale the first time somebody is in a hurry. Here the cost of stale
//! is specific. The gql-compat harness reads this file to decide what to
//! skip, so a declaration that no longer matches the engine turns real
//! verdicts into skips, or worse, produces passes for a graph nobody
//! asked about.

use std::path::PathBuf;
use std::process::Command;

/// The repo root, two levels up from `crates/zu-cli`.
fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("crates/zu-cli is two deep")
        .to_path_buf()
}

fn declared() -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_zu"))
        .args(["conformance", "--declare"])
        .output()
        .expect("run zu conformance --declare");
    assert!(
        out.status.success(),
        "zu conformance --declare failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).expect("the declaration is utf-8")
}

#[test]
fn conformance_toml_matches_the_declaration() {
    let rendered = declared();
    let path = repo_root().join("conformance.toml");
    if std::env::var_os("ZU_UPDATE_CONFORMANCE").is_some() {
        std::fs::write(&path, &rendered).expect("write conformance.toml");
        return;
    }
    // Checkout on Windows rewrites the line endings, so compare the
    // content rather than the bytes, the same way the GQLSTATUS drift
    // test and the TCK scoreboard do.
    let on_disk = std::fs::read_to_string(&path)
        .unwrap_or_default()
        .replace("\r\n", "\n");
    assert_eq!(
        on_disk, rendered,
        "conformance.toml is stale; rerun with ZU_UPDATE_CONFORMANCE=1"
    );
}

#[test]
fn the_declaration_is_toml_a_reader_can_parse() {
    // Not a full parser, just enough that a malformed render fails here
    // rather than in the harness. Every non-comment, non-blank line is
    // either a table header or a `key = value`, and the values are
    // exactly the spellings the harness accepts.
    let toml = declared();
    let mut tables = Vec::new();
    let mut in_list = false;
    for line in toml.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if in_list {
            if line == "]" {
                in_list = false;
            } else {
                assert!(
                    line.starts_with('"') && line.ends_with("\","),
                    "list entry is not a quoted string: {line:?}"
                );
            }
            continue;
        }
        if line.starts_with('[') {
            assert!(line.ends_with(']'), "unterminated table header: {line:?}");
            tables.push(line.to_string());
            continue;
        }
        let (key, value) = line.split_once(" = ").unwrap_or_else(|| {
            panic!("line is neither a table nor an assignment: {line:?}");
        });
        assert!(!key.is_empty(), "empty key in {line:?}");
        if value == "[" {
            in_list = true;
            continue;
        }
        // Strip the trailing reason comment before judging the value.
        let value = value.split("  #").next().expect("split always yields one");
        assert!(
            value == "true" || value == "false" || (value.starts_with('"') && value.ends_with('"')),
            "value is not a bool or a quoted string: {value:?} in {line:?}"
        );
    }
    assert!(!in_list, "the notes list was never closed");
    assert_eq!(tables, ["[engine]", "[data]", "[capabilities]"]);
}

#[test]
fn the_json_declaration_says_what_the_toml_says() {
    // The harness reads the JSON and a person reads the TOML, and the
    // whole arrangement is worthless if the two can disagree. They are
    // rendered from the same tables so they cannot, but that is an
    // invariant worth holding down rather than trusting.
    let out = Command::new(env!("CARGO_BIN_EXE_zu"))
        .args(["conformance", "--declare", "--format", "json"])
        .output()
        .expect("run zu conformance --declare --format json");
    assert!(out.status.success(), "--format json failed");
    let json = String::from_utf8(out.stdout).expect("utf-8");

    // Every `key = bool` in the TOML has to appear in the JSON with the
    // same bool. The reasons are TOML only, on purpose.
    let mut checked = 0;
    for line in declared().lines() {
        let line = line.trim();
        let Some((key, rest)) = line.split_once(" = ") else {
            continue;
        };
        let value = rest.split("  #").next().expect("split always yields one");
        if value != "true" && value != "false" {
            continue;
        }
        assert!(
            json.contains(&format!("\"{key}\":{value}")),
            "{key} is {value} in the TOML and not in the JSON: {json}"
        );
        checked += 1;
    }
    assert_eq!(
        checked, 20,
        "expected 15 data capabilities and 5 engine flags"
    );
}

#[test]
fn verify_accepts_a_report_that_agrees_and_rejects_one_that_does_not() {
    let dir = std::env::temp_dir().join(format!("zu-conf-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("mkdir");

    // A report that matches the declaration, trimmed to the fields
    // `--verify` reads. The harness writes far more than this and none
    // of the rest is any of zu's business.
    let agreeing = r#"{"engine":{"adapter":"zu","capabilities":{
        "Data":{"labels":true,"multi-label":false,"node-properties":true,
        "edge-properties":false,"edge-types":true,"multiple-edge-types":true,
        "multiple-node-labels":false,"temporal-values":false,"list-values":false,
        "null-properties":false,"float-values":true,"boolean-values":true,
        "undirected-edges":false,"self-loops":true,"parallel-edges":true},
        "GQLStatus":true,"Parameters":true,"Transactions":false,
        "MultipleStatements":true,"Isolated":true}},
        "cases":[{"id":"a","got_gqlstatus":"22012"}]}"#;
    let ok = dir.join("agree.json");
    std::fs::write(&ok, agreeing).expect("write");
    let out = Command::new(env!("CARGO_BIN_EXE_zu"))
        .args(["conformance", "--verify"])
        .arg(&ok)
        .output()
        .expect("run");
    assert!(
        out.status.success(),
        "a matching report was rejected: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // One flag flipped. This is the whole point: the adapter and the
    // engine live in different repositories and nothing else notices
    // when they part company.
    let drifted = agreeing.replace("\"float-values\":true", "\"float-values\":false");
    let bad = dir.join("drift.json");
    std::fs::write(&bad, &drifted).expect("write");
    let out = Command::new(env!("CARGO_BIN_EXE_zu"))
        .args(["conformance", "--verify"])
        .arg(&bad)
        .output()
        .expect("run");
    assert!(!out.status.success(), "drift was accepted");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("float-values"), "got {stderr}");

    // A claim that did nothing. zu says it reports GQLSTATUS, and not
    // one case in the run was graded on a code, so the claim is empty
    // and the report should say so rather than print a tick.
    let empty = agreeing.replace(r#"{"id":"a","got_gqlstatus":"22012"}"#, r#"{"id":"a"}"#);
    let hollow = dir.join("hollow.json");
    std::fs::write(&hollow, &empty).expect("write");
    let out = Command::new(env!("CARGO_BIN_EXE_zu"))
        .args(["conformance", "--verify"])
        .arg(&hollow)
        .output()
        .expect("run");
    assert!(
        !out.status.success(),
        "an empty gqlstatus claim was accepted"
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("no case in the report was graded"),
        "got {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // A capability the report never mentions. Not declared is not "no":
    // this has to be as loud as a contradiction, or an adapter that
    // forgets a flag is indistinguishable from one that said no.
    let silent = agreeing.replace("\"self-loops\":true,", "");
    let quiet = dir.join("silent.json");
    std::fs::write(&quiet, &silent).expect("write");
    let out = Command::new(env!("CARGO_BIN_EXE_zu"))
        .args(["conformance", "--verify"])
        .arg(&quiet)
        .output()
        .expect("run");
    assert!(!out.status.success(), "a silent capability was accepted");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("reported nothing at all"),
        "got {}",
        String::from_utf8_lossy(&out.stderr)
    );

    std::fs::remove_dir_all(&dir).ok();
}
