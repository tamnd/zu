//! `zu corpus`: runs the cross-client conformance corpus against this
//! engine.
//!
//! The corpus lives in `conformance/cases` in this repository and ships
//! as a release artifact every client repository consumes (ADR 0005).
//! This command is what a person runs against an unpacked artifact, and
//! it is also the reference the other eight runners are graded against:
//! when a runner in another language disagrees with this one about a
//! case, this one is right by definition, because the cases and the
//! engine version together are the contract.
//!
//! It is separate from `zu conformance`, which is about the gql-compat
//! harness and the capability declaration. The two share a word and
//! nothing else: that one says what this engine can do, this one says
//! what a value means on the way out.
//!
//! Exit code 1 on any failure, so a script can gate on it. An
//! unsupported case is not a failure here, because the corpus is
//! allowed to run ahead of the engine, but the count is printed either
//! way and `--strict` turns it into one.

use std::path::Path;
use std::process::ExitCode;

use zu_corpus::{Outcome, load, run};

/// Parses the argument list and runs the corpus in `dir`.
pub(crate) fn corpus_command(args: &[String]) -> ExitCode {
    let mut dir: Option<&str> = None;
    let mut strict = false;
    let mut quiet = false;
    for arg in args {
        match arg.as_str() {
            "--strict" => strict = true,
            "--quiet" | "-q" => quiet = true,
            arg if arg.starts_with('-') => return crate::usage_error("corpus"),
            arg if dir.is_none() => dir = Some(arg),
            _ => return crate::usage_error("corpus"),
        }
    }
    let Some(dir) = dir else {
        return crate::usage_error("corpus");
    };
    corpus(Path::new(dir), strict, quiet)
}

/// Loads every suite, runs every case, and prints what happened.
///
/// The scratch databases go in a temporary directory that is removed
/// when the run ends, because a case gets a database of its own and a
/// corpus of a few hundred cases would otherwise leave a few hundred
/// files behind wherever it was run from.
fn corpus(dir: &Path, strict: bool, quiet: bool) -> ExitCode {
    let suites = match load(dir) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("zu corpus: {e}");
            return ExitCode::FAILURE;
        }
    };
    let temp = match tempdir() {
        Ok(t) => t,
        Err(e) => {
            eprintln!("zu corpus: {e}");
            return ExitCode::FAILURE;
        }
    };
    let report = run(&suites, &temp);
    let _ = std::fs::remove_dir_all(&temp);

    if !quiet {
        // Everything that did not pass gets a line, in the order the
        // cases ran, so the first line of output is the first thing
        // that went wrong rather than the last.
        for ran in &report.ran {
            if ran.outcome != Outcome::Passed {
                println!("{ran}");
            }
        }
    }
    println!("{}", report.summary());

    let unsupported = report.count(Outcome::Unsupported);
    if report.count(Outcome::Failed) > 0 || (strict && unsupported > 0) {
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

/// A directory nothing else is writing to, under the system temporary
/// directory.
///
/// `tempfile` is a dev-dependency here and the CLI is capped at 15 MiB
/// (T7), so this does the one thing the command needs rather than
/// pulling the crate into the shipped binary. The process id keeps two
/// runs on one machine apart and the counter keeps two runs in one
/// process apart, which is not how the command is used and is how the
/// tests below use it.
fn tempdir() -> Result<std::path::PathBuf, String> {
    use std::sync::atomic::{AtomicU32, Ordering};
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!("zu-corpus-{}-{n}", std::process::id()));
    // A previous run killed before it cleaned up leaves this behind,
    // and its stale databases would be opened rather than created.
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).map_err(|e| format!("creating {}: {e}", path.display()))?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEAD: &str = "schema: 1\nsuite: t\ndoc: a suite the command's own tests run\n\ncases:\n";

    /// Writes a one-suite corpus and returns the directory holding it.
    /// The file is named `t.yaml` because `load` checks the suite's name
    /// against the file's.
    fn corpus_dir(cases: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("a temp dir");
        std::fs::write(dir.path().join("t.yaml"), format!("{HEAD}{cases}")).expect("write");
        dir
    }

    #[test]
    fn a_corpus_that_passes_exits_zero() {
        let dir = corpus_dir(
            "  - name: one\n    doc: the smallest statement there is\n    query: RETURN 1 AS n\n    columns:\n      - n\n    rows:\n      - values:\n          - type: INT8\n            value: 1\n",
        );
        assert_eq!(corpus(dir.path(), false, true), ExitCode::SUCCESS);
    }

    #[test]
    fn a_failing_case_exits_one_so_a_script_can_gate_on_it() {
        let dir = corpus_dir(
            "  - name: wrong\n    doc: a case asserting an answer the engine does not give\n    query: RETURN 1 AS n\n    columns:\n      - n\n    rows:\n      - values:\n          - type: INT8\n            value: 2\n",
        );
        assert_eq!(corpus(dir.path(), false, true), ExitCode::FAILURE);
    }

    /// The corpus is allowed to run ahead of the engine, so a statement
    /// this engine does not implement is not a failure by default. It is
    /// one under `--strict`, which is what a release runs, because at a
    /// release the two are supposed to have met.
    #[test]
    fn a_case_ahead_of_the_engine_passes_until_strict_says_otherwise() {
        let dir = corpus_dir(
            "  - name: ahead\n    doc: a case written ahead of the engine, which is allowed on purpose\n    query: START TRANSACTION\n    columns:\n      - n\n    rows:\n",
        );
        assert_eq!(corpus(dir.path(), false, true), ExitCode::SUCCESS);
        assert_eq!(corpus(dir.path(), true, true), ExitCode::FAILURE);
    }

    #[test]
    fn a_directory_holding_no_cases_is_an_error_rather_than_an_empty_pass() {
        let dir = tempfile::tempdir().expect("a temp dir");
        assert_eq!(corpus(dir.path(), false, true), ExitCode::FAILURE);
        assert_eq!(
            corpus(&dir.path().join("absent"), false, true),
            ExitCode::FAILURE
        );
    }

    #[test]
    fn the_argument_list_takes_the_directory_and_the_two_flags_in_any_order() {
        let dir = corpus_dir(
            "  - name: one\n    doc: the smallest statement there is\n    query: RETURN 1 AS n\n    columns:\n      - n\n    rows:\n      - values:\n          - type: INT8\n            value: 1\n",
        );
        let path = dir.path().display().to_string();
        let args = |v: &[&str]| -> Vec<String> { v.iter().map(|s| (*s).to_owned()).collect() };
        assert_eq!(
            corpus_command(&args(&["--quiet", &path, "--strict"])),
            ExitCode::SUCCESS
        );
        // No directory is a usage error and not a run of the working
        // directory, which holds no cases and would report as one. A
        // usage error is 2 and a run that found something wrong is 1, so
        // a script can tell "you called this wrongly" from "the corpus
        // does not pass".
        assert_eq!(corpus_command(&args(&["--quiet"])), ExitCode::from(2));
        assert_eq!(
            corpus_command(&args(&[&path, "--verbose"])),
            ExitCode::from(2),
            "an unknown flag is a usage error rather than a path"
        );
    }
}
