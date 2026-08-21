//! The committed corpus, run against this engine.
//!
//! This is the gate. The module tests beside the code prove the reader
//! and the runner behave; this proves the cases in the tree are cases
//! this engine answers, and it is what "the corpus runs in Rust" comes
//! to. It is a test rather than only a CI job so that it runs on the
//! machine of whoever adds a case, which is the only place the answer
//! is cheap.

use std::path::{Path, PathBuf};

use zu_corpus::{Outcome, load, run};

fn dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../conformance/cases")
}

fn suites() -> Vec<zu_corpus::Suite> {
    load(&dir()).expect("every file in the corpus is a suite")
}

#[test]
fn every_case_in_the_corpus_is_one_this_engine_answers() {
    let suites = suites();
    let temp = tempfile::tempdir().expect("a temp dir");
    let report = run(&suites, temp.path());

    let failures: Vec<String> = report.failures().map(ToString::to_string).collect();
    assert!(
        failures.is_empty(),
        "{}\n{}",
        report.summary(),
        failures.join("\n")
    );
    // A corpus that ran nothing passes every assertion above it, which
    // is the one way this test could be green and mean nothing.
    assert!(report.count(Outcome::Passed) > 0, "{}", report.summary());
}

#[test]
fn nothing_in_the_corpus_is_ahead_of_the_engine_without_it_being_noticed() {
    let temp = tempfile::tempdir().expect("a temp dir");
    let report = run(&suites(), temp.path());
    // A case the engine does not implement is allowed by the runner on
    // purpose, because the corpus is the contract and the engine
    // catches up to it. It is not allowed here without being written
    // down, because an unsupported case is one nobody is measuring and
    // the number of them only ever goes up quietly.
    let ahead: Vec<String> = report
        .ran
        .iter()
        .filter(|r| r.outcome == Outcome::Unsupported)
        .map(ToString::to_string)
        .collect();
    assert!(ahead.is_empty(), "{}", ahead.join("\n"));
}

#[test]
fn every_case_says_why_it_is_here_in_a_sentence_somebody_wrote() {
    for suite in suites() {
        assert!(
            suite.doc.len() > 40,
            "the {} suite does not say what it covers",
            suite.name
        );
        for case in suite.cases {
            // The number is a floor on effort and not on quality, but
            // a `doc` of "int64" is one nobody can decide the fate of
            // when the case starts failing, and that is the moment the
            // sentence is for.
            assert!(
                case.doc.len() > 40,
                "{}/{} line {}: `doc:` is {:?}, which does not say why the case is here",
                suite.name,
                case.name,
                case.line,
                case.doc
            );
            assert!(
                case.doc.ends_with('.'),
                "{}/{} line {}: `doc:` is a sentence and ends in a full stop",
                suite.name,
                case.name,
                case.line
            );
        }
    }
}

#[test]
fn the_corpus_holds_more_than_one_suite_and_they_load_in_a_fixed_order() {
    let names: Vec<String> = suites().into_iter().map(|s| s.name).collect();
    let mut sorted = names.clone();
    sorted.sort();
    assert_eq!(names, sorted, "suites load in name order on every machine");
    assert!(names.len() > 1, "{names:?}");
}

#[test]
fn some_of_the_corpus_stores_a_value_before_it_reads_one_back() {
    // The expression cases would all still pass with the load path
    // broken, or deleted, or never applied, because none of them reads
    // anything back. This is the assertion that the other half of the
    // question is still being asked at all.
    let suites = suites();
    let loaded: Vec<&str> = suites
        .iter()
        .filter(|s| s.load.is_some())
        .map(|s| s.name.as_str())
        .collect();
    assert!(!loaded.is_empty(), "no suite has a `load:`");
    let cases: usize = suites
        .iter()
        .filter(|s| s.load.is_some())
        .map(|s| s.cases.len())
        .sum();
    assert!(cases > 20, "{loaded:?} hold {cases} cases between them");
}

#[test]
fn a_file_whose_name_and_suite_disagree_is_refused_rather_than_reported_under_one_of_them() {
    let temp = tempfile::tempdir().expect("a temp dir");
    std::fs::write(
        temp.path().join("named.yaml"),
        "schema: 4\nsuite: other\ndoc: a suite whose file says one name and whose text says another\ncases:\n  - name: a\n    doc: a case, which this test never gets as far as running\n    query: RETURN 1 AS n\n    raises: \"22012\"\n",
    )
    .expect("writing the fixture");
    let err = load(temp.path()).expect_err("refused");
    assert!(err.contains("calls itself \"other\""), "{err}");
}
