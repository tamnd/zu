//! Running the corpus, and saying what happened in a form the other
//! eight runners can be compared against.
//!
//! Each case gets a database of its own. Cases in a suite are written
//! as if nothing came before them, and the cheapest way to keep that
//! true is to make it true: a case that leaked a table into the next
//! one would be a failure that moves when the file is reordered, which
//! is the worst kind to be handed.
//!
//! An outcome is one of three things and not two. Passed and failed
//! are obvious. Unsupported is the third, and it exists because the
//! corpus is versioned with the engine and shipped to nine clients
//! that will not all implement the same subset at the same time: a
//! client that cannot yet parse a statement should say so, and the
//! report should be able to tell that apart from an answer that came
//! back wrong. Here that is a statement the engine refuses as
//! unsupported syntax, which is a fact about zu today rather than a
//! defect in the case.

use std::fmt;
use std::path::Path;

use zu::query::Value;
use zu::session::Session;

use crate::case::{Case, Expect, Suite};
use crate::value::{Cell, Tables, from_engine, same, show};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Passed,
    Failed,
    Unsupported,
}

/// What one case did, with the account of why when it did not pass.
#[derive(Debug, Clone)]
pub struct Ran {
    pub suite: String,
    pub case: String,
    pub line: usize,
    pub outcome: Outcome,
    /// What went wrong, in enough detail to fix the case or the engine
    /// without running it again.
    pub detail: String,
}

impl fmt::Display for Ran {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mark = match self.outcome {
            Outcome::Passed => "ok",
            Outcome::Failed => "FAILED",
            Outcome::Unsupported => "unsupported",
        };
        write!(f, "{}/{} line {} {mark}", self.suite, self.case, self.line)?;
        match self.detail.is_empty() {
            true => Ok(()),
            false => write!(f, ": {}", self.detail),
        }
    }
}

/// What a whole run did.
#[derive(Debug, Clone, Default)]
pub struct Report {
    pub ran: Vec<Ran>,
}

impl Report {
    pub fn count(&self, outcome: Outcome) -> usize {
        self.ran.iter().filter(|r| r.outcome == outcome).count()
    }

    pub fn failures(&self) -> impl Iterator<Item = &Ran> {
        self.ran.iter().filter(|r| r.outcome == Outcome::Failed)
    }

    /// One line saying what the run came to, which is what a CI log
    /// keeps and what two runs are compared by.
    pub fn summary(&self) -> String {
        format!(
            "{} cases, {} passed, {} failed, {} unsupported",
            self.ran.len(),
            self.count(Outcome::Passed),
            self.count(Outcome::Failed),
            self.count(Outcome::Unsupported)
        )
    }
}

/// Runs every case of every suite, in the order they were written.
///
/// `dir` is a directory the runner may make databases under. Each case
/// gets its own file in it, named after the case, so that a failure
/// leaves something to open.
pub fn run(suites: &[Suite], dir: &Path) -> Report {
    let mut report = Report::default();
    for suite in suites {
        for case in &suite.cases {
            report.ran.push(one(suite, case, dir));
        }
    }
    report
}

fn one(suite: &Suite, case: &Case, dir: &Path) -> Ran {
    let ran = |outcome, detail: String| Ran {
        suite: suite.name.clone(),
        case: case.name.clone(),
        line: case.line,
        outcome,
        detail,
    };
    let path = dir.join(format!("{}-{}.zu", suite.name, case.name));
    let mut file = match zu::zu1::file::Zu1File::create(&path) {
        Ok(file) => file,
        Err(e) => return ran(Outcome::Failed, format!("creating {}: {e}", path.display())),
    };
    // The load goes in before the session opens, because it is bulk
    // load and bulk load is the path that owns the file rather than
    // one that goes through a statement. Every case of the suite gets
    // its own copy of it for the same reason every case gets its own
    // database: a case that read what the case before it wrote would
    // be a failure that moves when the file is reordered.
    if let Some(load) = &suite.load
        && let Err(e) = load.apply(&mut file)
    {
        return ran(Outcome::Failed, format!("the suite's load: {e}"));
    }
    drop(file);
    let mut session = match Session::open(&path) {
        Ok(session) => session,
        Err(e) => return ran(Outcome::Failed, format!("opening {}: {e}", path.display())),
    };

    for (i, statement) in case.setup.iter().enumerate() {
        if let Err(e) = session.run(statement, &[]) {
            // A setup that fails is not a result about the statement
            // under test, so it is never a pass and never a quiet skip.
            return match unsupported(&e) {
                true => ran(Outcome::Unsupported, format!("setup {}: {e}", i + 1)),
                false => ran(Outcome::Failed, format!("setup {} failed: {e}", i + 1)),
            };
        }
    }

    // The engine takes the values by value and the case owns them, so
    // this is where they are copied. It is a copy per case rather than
    // per row, which is nothing against opening a database.
    let params: Vec<(&str, Value)> = case
        .params
        .iter()
        .map(|(name, value)| (name.as_str(), value.clone()))
        .collect();
    let result = session.run(&case.query, &params);
    match (&case.expect, result) {
        (Expect::Raises(code), Err(e)) => match e.gqlstatus().map(|s| s.code()) {
            Some(got) if got == code => ran(Outcome::Passed, String::new()),
            Some(got) => ran(
                Outcome::Failed,
                format!("raised {got} where the case wants {code}: {e}"),
            ),
            None => ran(
                Outcome::Failed,
                format!("failed with no GQLSTATUS where the case wants {code}: {e}"),
            ),
        },
        (Expect::Raises(code), Ok(_)) => ran(
            Outcome::Failed,
            format!("returned rows where the case wants {code}"),
        ),
        (Expect::Rows { .. }, Err(e)) if unsupported(&e) => {
            ran(Outcome::Unsupported, e.to_string())
        }
        (Expect::Rows { .. }, Err(e)) => ran(Outcome::Failed, e.to_string()),
        (Expect::Rows { columns, rows }, Ok(got)) => {
            // The catalog after the statement rather than before it,
            // because a statement may have made the table the rows it
            // returns are rows of.
            let tables = Named(session.catalog());
            match compare(columns, rows, &got, &tables) {
                None => ran(Outcome::Passed, String::new()),
                Some(detail) => ran(Outcome::Failed, detail),
            }
        }
    }
}

/// The names of the tables the case ran against, which is what turns a
/// node value into the spelling a case is written in.
struct Named<'a>(&'a zu::zu1::catalog::Catalog);

impl Tables for Named<'_> {
    fn name(&self, table: u32) -> Option<&str> {
        self.0
            .node_by_id(table)
            .map(|t| t.name.as_str())
            .or_else(|| self.0.rel_by_id(table).map(|t| t.name.as_str()))
    }
}

/// Whether an error means the engine does not implement the statement
/// rather than that the statement is wrong.
///
/// The two GQL classes that say so are 42, syntax error or access rule
/// violation, and 0A, feature not supported. A case landing on either
/// is a case ahead of the engine, which the corpus allows on purpose:
/// the cases are the contract and the engine catches up to them.
fn unsupported(e: &zu::ZuError) -> bool {
    e.gqlstatus()
        .map(|s| s.code())
        .is_some_and(|code| code.starts_with("42") || code.starts_with("0A"))
}

/// What differs between what a case wants and what came back, or
/// `None` if nothing does.
///
/// It reports the first difference rather than all of them, because
/// the first is nearly always the cause of the rest, and a report that
/// prints a hundred rows is one nobody reads to the end.
fn compare(
    columns: &[String],
    rows: &[Vec<Cell>],
    got: &zu::query::QueryResult,
    tables: &dyn Tables,
) -> Option<String> {
    if columns != got.columns.as_slice() {
        return Some(format!(
            "columns {:?} where the case wants {columns:?}",
            got.columns
        ));
    }
    for (i, (want, found)) in rows.iter().zip(&got.rows).enumerate() {
        for (j, (want, found)) in want.iter().zip(found).enumerate() {
            let found = from_engine(found, tables);
            if !same(want, &found) {
                return Some(format!(
                    "row {} column {} is {} where the case wants {}",
                    i + 1,
                    columns.get(j).map_or("?", String::as_str),
                    show(&found),
                    show(want)
                ));
            }
        }
    }
    if rows.len() != got.rows.len() {
        return Some(format!(
            "{} rows where the case wants {}",
            got.rows.len(),
            rows.len()
        ));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::case::Suite;

    const HEAD: &str = "schema: 3\nsuite: t\ndoc: a suite for the runner's own tests\n\ncases:\n";

    fn run_cases(cases: &str) -> Report {
        let suite = Suite::parse(&format!("{HEAD}{cases}")).expect("the fixture parses");
        let dir = tempfile::tempdir().expect("a temp dir");
        run(&[suite], dir.path())
    }

    #[test]
    fn a_case_whose_rows_come_back_passes() {
        let report = run_cases(
            "  - name: one\n    doc: the smallest statement there is\n    query: RETURN 1 AS n\n    columns:\n      - n\n    rows:\n      - values:\n          - type: INT8\n            value: 1\n",
        );
        assert_eq!(report.count(Outcome::Passed), 1, "{}", report.summary());
        assert_eq!(report.count(Outcome::Failed), 0);
    }

    #[test]
    fn a_case_whose_value_differs_says_which_row_and_column_and_both_values() {
        let report = run_cases(
            "  - name: wrong\n    doc: a case asserting the wrong answer, to see what it says\n    query: RETURN 1 AS n\n    columns:\n      - n\n    rows:\n      - values:\n          - type: INT8\n            value: 2\n",
        );
        let failure = report.failures().next().expect("one failure");
        assert!(
            failure
                .detail
                .contains("row 1 column n is INT64 \"1\" where the case wants INT64 \"2\""),
            "{}",
            failure.detail
        );
        assert!(failure.to_string().contains("t/wrong"), "{failure}");
    }

    #[test]
    fn a_case_whose_columns_differ_is_a_different_complaint_from_a_value() {
        let report = run_cases(
            "  - name: named\n    doc: the projected name is part of the answer\n    query: RETURN 1 AS n\n    columns:\n      - m\n    rows:\n      - values:\n          - type: INT8\n            value: 1\n",
        );
        let failure = report.failures().next().expect("one failure");
        assert!(
            failure.detail.starts_with("columns [\"n\"]"),
            "{}",
            failure.detail
        );
    }

    #[test]
    fn a_case_getting_more_rows_than_it_asked_for_is_a_failure_and_not_a_prefix_match() {
        let report = run_cases(
            "  - name: two\n    doc: two rows where the case writes one\n    query: UNWIND [1, 2] AS n RETURN n\n    columns:\n      - n\n    rows:\n      - values:\n          - type: INT8\n            value: 1\n",
        );
        let failure = report.failures().next().expect("one failure");
        assert!(
            failure.detail.contains("2 rows where the case wants 1"),
            "{}",
            failure.detail
        );
    }

    #[test]
    fn a_case_wanting_a_condition_passes_on_that_condition_and_not_another() {
        let report = run_cases(
            "  - name: right-code\n    doc: dividing by zero raises 22012\n    query: RETURN 1 / 0\n    raises: 22012\n  - name: wrong-code\n    doc: the same statement, against a code it does not raise\n    query: RETURN 1 / 0\n    raises: 22003\n",
        );
        assert_eq!(report.ran[0].outcome, Outcome::Passed, "{}", report.ran[0]);
        assert_eq!(report.ran[1].outcome, Outcome::Failed);
        assert!(
            report.ran[1].detail.contains("raised 22012"),
            "{}",
            report.ran[1].detail
        );
    }

    #[test]
    fn a_case_wanting_a_condition_that_gets_rows_instead_is_a_failure() {
        let report = run_cases(
            "  - name: no-raise\n    doc: a statement that does not raise, against a case that says it does\n    query: RETURN 1 AS n\n    raises: 22012\n",
        );
        assert!(
            report.ran[0]
                .detail
                .contains("returned rows where the case wants 22012"),
            "{}",
            report.ran[0].detail
        );
    }

    #[test]
    fn a_case_with_parameters_runs_the_statement_with_them_bound() {
        let report = run_cases(
            "  - name: bound\n    doc: two parameters, one of each of the two encodings\n    params:\n      - name: n\n        type: INT64\n        value: \"42\"\n      - name: s\n        type: STRING\n        value: ada\n    query: RETURN $n + 1 AS n, $s AS s\n    columns:\n      - n\n      - s\n    rows:\n      - values:\n          - type: INT64\n            value: \"43\"\n          - type: STRING\n            value: ada\n",
        );
        assert_eq!(report.ran[0].outcome, Outcome::Passed, "{}", report.ran[0]);
    }

    #[test]
    fn a_statement_wanting_a_parameter_the_case_did_not_bind_is_a_condition_like_any_other() {
        // 42002 is what the engine answers a reference that resolves to
        // nothing, and a parameter nobody bound is one. It is a case
        // worth writing on purpose, so the runner has to let a case
        // assert it rather than treating every 42 as a statement the
        // engine has not implemented.
        let report = run_cases(
            "  - name: missing\n    doc: a statement whose parameter nobody bound\n    query: RETURN $n AS n\n    raises: 42002\n",
        );
        assert_eq!(report.ran[0].outcome, Outcome::Passed, "{}", report.ran[0]);
    }

    #[test]
    fn the_setup_of_a_case_does_not_see_its_parameters() {
        // A parameter belongs to the one statement under test. A setup
        // that could take one would be a second statement under test
        // wearing another name, and this is the assertion that says so
        // out loud rather than leaving it to be discovered.
        let report = run_cases(
            "  - name: setup-unbound\n    doc: a setup statement naming the parameter the query binds\n    setup:\n      - RETURN $n AS n\n    params:\n      - name: n\n        type: INT8\n        value: 1\n    query: RETURN $n AS n\n    columns:\n      - n\n    rows:\n      - values:\n          - type: INT8\n            value: 1\n",
        );
        assert_ne!(report.ran[0].outcome, Outcome::Passed, "{}", report.ran[0]);
        assert!(
            report.ran[0].detail.contains("setup 1: 42002"),
            "{}",
            report.ran[0].detail
        );
    }

    #[test]
    fn a_statement_the_engine_does_not_implement_is_its_own_outcome() {
        let report = run_cases(
            "  - name: ahead\n    doc: a case written ahead of the engine, which is allowed on purpose\n    query: CREATE (n:person)\n    columns:\n      - n\n    rows:\n",
        );
        assert_eq!(
            report.ran[0].outcome,
            Outcome::Unsupported,
            "{}",
            report.ran[0]
        );
        assert_eq!(report.count(Outcome::Failed), 0);
    }

    #[test]
    fn each_case_gets_a_database_of_its_own() {
        let report = run_cases(
            "  - name: creates\n    doc: a case that leaves a row behind it\n    setup:\n      - \"INSERT (n:Leak {name: 'x'})\"\n    query: MATCH (n:Leak) RETURN count(n) AS n\n    columns:\n      - n\n    rows:\n      - values:\n          - type: INT8\n            value: 1\n  - name: sees-nothing\n    doc: the next case must not see the row the one above wrote\n    query: MATCH (n:Leak) RETURN count(n) AS n\n    columns:\n      - n\n    rows:\n      - values:\n          - type: INT8\n            value: 0\n",
        );
        // The second case counts what only the first case wrote. A
        // label the graph does not hold counts nothing, so a database
        // that leaked would answer one here and the case would fail.
        assert_eq!(report.ran[0].outcome, Outcome::Passed, "{}", report.ran[0]);
        assert_eq!(report.ran[1].outcome, Outcome::Passed, "{}", report.ran[1]);
    }

    #[test]
    fn a_run_says_what_it_came_to_in_one_line() {
        let report = run_cases(
            "  - name: a\n    doc: one that passes\n    query: RETURN 1 AS n\n    columns:\n      - n\n    rows:\n      - values:\n          - type: INT8\n            value: 1\n  - name: b\n    doc: one that does not\n    query: RETURN 1 AS n\n    columns:\n      - n\n    rows:\n      - values:\n          - type: INT8\n            value: 2\n",
        );
        assert_eq!(
            report.summary(),
            "2 cases, 1 passed, 1 failed, 0 unsupported"
        );
    }
}
