//! Every scalar function is a vector kernel (G7, GF01 through GF07,
//! ISO 20.x). That is a claim about the plan and not only about the
//! answer, so it needs a test that reads the plan: a function whose
//! kernel exists but whose shape the pipeline declines is a function
//! the query still evaluates a row at a time, and nothing about the
//! rows it returns says which of the two happened.
//!
//! `EXPLAIN ANALYZE` prints a `decisions:` section exactly when the
//! pipeline executor took the plan, so that word is the whole test.
//! Every case below is written the same way, one function over one
//! column of a small graph, because what is under test is the compiler
//! and not the arithmetic; the kernels have their own unit tests in
//! `zu-vector` and the answers have theirs in the conformance corpus.
//!
//! The register at the bottom is the other half. Three functions
//! answer a boolean and the pipeline has no boolean column, so they
//! compile where a predicate goes and nowhere else, and three more
//! read a list or a path, neither of which the pipeline has a
//! representation for. Writing them down here is what keeps the claim
//! honest: the line says which functions are kernels and this file
//! says which ones are not and why.

use zu::query::Value;
use zu::session::Session;
use zu::zu1::file::Zu1File;
use zu::zu1::graph::bulk_load_as;
use zu::zu1::props::{PropValues, store_props};
use zu::{Engine, Options};

/// Four people in a ring of `knows`, each with a whole number, a
/// number with a fraction and a name. Every height is above nought and
/// inside minus one to one, so every function in the table has an
/// answer for every row and no case here is about a condition.
fn opened(name: &str, heights: &[f64]) -> (tempfile::TempDir, Session) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join(name);
    let mut db = Zu1File::create(&path).expect("create");
    let edges: Vec<(u32, u32)> = (0..4).map(|i| (i, (i + 1) % 4)).collect();
    bulk_load_as(&mut db, "person", "knows", 4, &edges).expect("load");
    let ages: Vec<u64> = (1..=4).collect();
    let names: Vec<&[u8]> = vec![b"ana", b"bo", b"cyd", b"dee"];
    store_props(
        &mut db,
        "person",
        &[
            ("age", PropValues::Int(&ages)),
            ("height", PropValues::Float(heights)),
            ("name", PropValues::Str(&names)),
        ],
    )
    .expect("props");
    drop(db);
    let session = Session::open(&path).expect("open");
    (dir, session)
}

const SAFE: &[f64] = &[0.125, 0.25, 0.5, 0.75];

/// The same four people with the last height below nought, which is
/// the value a root and a logarithm have no answer for. It is last so
/// that a query reading one row reads a row that does have an answer.
const RAISES: &[f64] = &[0.125, 0.25, 0.5, -1.0];

fn compiled(session: &mut Session, source: &str) -> bool {
    session
        .explain_analyze(source, &[])
        .unwrap_or_else(|e| panic!("{source}: {e}"))
        .contains("decisions:")
}

fn rows(session: &mut Session, source: &str) -> Vec<Vec<Value>> {
    session
        .run(source, &[])
        .unwrap_or_else(|e| panic!("{source}: {e}"))
        .rows
        .to_vec()
}

/// The same statement pinned to the row executor, which is the oracle
/// every kernel answer is compared against. A switch on the session
/// rather than a variable in the environment, for the reason #513
/// records: the environment belongs to the process and the tests in
/// this binary run in parallel.
fn on_rows(session: &mut Session, source: &str) -> Vec<Vec<Value>> {
    let was = session.options().clone();
    session.set_options(Options {
        engine: Engine::Rows,
        ..was.clone()
    });
    let answer = session.run(source, &[]);
    session.set_options(was);
    answer
        .unwrap_or_else(|e| panic!("{source} on rows: {e}"))
        .rows
        .to_vec()
}

fn status(session: &mut Session, source: &str) -> String {
    let err = session
        .run(source, &[])
        .expect_err(&format!("{source} should have raised"));
    err.gqlstatus()
        .unwrap_or_else(|| panic!("{source} raised {err}, which carries no gqlstatus"))
        .code()
        .to_string()
}

/// GF01 numeric, GF02 trigonometric, GF03 logarithmic, GF05 through
/// GF07 the trim family, and the string and element functions beside
/// them. One projection each, since a projection is the shape that
/// makes the vector: the column is computed where the level is built
/// and read back the way a stored column is read.
const KERNELS: &[(&str, &str)] = &[
    // GF01, whole numbers and roundings.
    ("abs", "RETURN abs(p.age) AS v"),
    ("ceil", "RETURN ceil(p.height) AS v"),
    ("floor", "RETURN floor(p.height) AS v"),
    ("round", "RETURN round(p.height) AS v"),
    ("sign", "RETURN sign(p.age) AS v"),
    ("mod", "RETURN mod(p.age, 3) AS v"),
    ("sqrt", "RETURN sqrt(p.height) AS v"),
    ("power", "RETURN power(p.height, 2.0) AS v"),
    // GF03, the logarithmic family.
    ("exp", "RETURN exp(p.height) AS v"),
    ("ln", "RETURN ln(p.height) AS v"),
    ("log", "RETURN log(2.0, p.height) AS v"),
    ("log10", "RETURN log10(p.height) AS v"),
    // GF02, the trigonometric family and the two angle conversions.
    ("sin", "RETURN sin(p.height) AS v"),
    ("cos", "RETURN cos(p.height) AS v"),
    ("tan", "RETURN tan(p.height) AS v"),
    ("cot", "RETURN cot(p.height) AS v"),
    ("asin", "RETURN asin(p.height) AS v"),
    ("acos", "RETURN acos(p.height) AS v"),
    ("atan", "RETURN atan(p.height) AS v"),
    ("degrees", "RETURN degrees(p.height) AS v"),
    ("radians", "RETURN radians(p.height) AS v"),
    // The lengths, which answer a count of something in a string.
    ("char_length", "RETURN char_length(p.name) AS v"),
    ("octet_length", "RETURN octet_length(p.name) AS v"),
    ("size", "RETURN size(p.name) AS v"),
    // The folds.
    ("upper", "RETURN upper(p.name) AS v"),
    ("lower", "RETURN lower(p.name) AS v"),
    // GF05 through GF07, the trim family and the two cuts.
    ("trim", "RETURN trim(p.name) AS v"),
    ("trim_leading", "RETURN trim(LEADING 'a' FROM p.name) AS v"),
    ("btrim", "RETURN btrim(p.name, 'a') AS v"),
    ("left", "RETURN left(p.name, 2) AS v"),
    ("right", "RETURN right(p.name, 2) AS v"),
    // Normalization and the two element functions.
    ("normalize", "RETURN normalize(p.name) AS v"),
    ("id", "RETURN id(p) AS v"),
    ("element_id", "RETURN element_id(p) AS v"),
];

/// The three that answer a boolean. A computed column carries an
/// integer, a float, a string or a temporal value, and there is no
/// boolean among them, so a projection of one of these has nowhere to
/// land. Compiled into a predicate they are kernels like any other,
/// which is the shape they are written in anyway.
const PREDICATES: &[(&str, &str)] = &[
    ("is_normalized", "p.name IS NORMALIZED"),
    ("all_different", "all_different(p, p)"),
    ("same", "same(p, p)"),
];

/// What has no kernel, and why. Each of these reads a value the
/// pipeline has no vector for: `PhysType::List` exists but nothing
/// produces one and `clone_aux` still refuses to compact one, and a
/// path is not a column at all. The queries run and answer correctly
/// on the row executor, so this is a plan the pipeline hands back and
/// not a feature that is missing.
const UNCOVERED: &[(&str, &str, &str)] = &[
    (
        "cardinality",
        "MATCH (p:person) RETURN cardinality([p.age, p.age]) AS v",
        "GF12, and exec2 has no list vector to count the elements of",
    ),
    (
        "path_length",
        "MATCH q = (a:person)-[:knows]->(b:person) RETURN path_length(q) AS v",
        "GF04, and a path is not a column",
    ),
];

/// The whole of the line: every function in the table above is a
/// kernel over a morsel, so the plan that answers it is the compiled
/// pipeline and not a walk of the rows.
#[test]
fn every_scalar_function_is_a_kernel_the_pipeline_compiles() {
    let (_dir, mut session) = opened("kernels-compile.zu1", SAFE);
    let mut per_row = Vec::new();
    for (what, tail) in KERNELS {
        let source = format!("MATCH (p:person) {tail}");
        if !compiled(&mut session, &source) {
            per_row.push(*what);
        }
    }
    assert!(
        per_row.is_empty(),
        "these went to the row executor: {per_row:?}"
    );
}

/// And it answers what the row executor answers, row for row. A kernel
/// that compiled and got the arithmetic wrong would pass the test
/// above, so the same list runs twice and the two are compared.
#[test]
fn a_kernel_answers_what_the_row_executor_answers() {
    let (_dir, mut session) = opened("kernels-agree.zu1", SAFE);
    for (what, tail) in KERNELS {
        let source = format!("MATCH (p:person) {tail} ORDER BY v");
        let compiled = rows(&mut session, &source);
        assert_eq!(compiled.len(), 4, "{what} answered {compiled:?}");
        assert_eq!(compiled, on_rows(&mut session, &source), "{what}");
    }
}

/// The three boolean ones, where a boolean goes. A predicate register
/// is not a column, so these compile in a WHERE and the pipeline takes
/// the plan.
#[test]
fn the_boolean_functions_are_kernels_where_a_predicate_goes() {
    let (_dir, mut session) = opened("kernels-pred.zu1", SAFE);
    for (what, pred) in PREDICATES {
        let source = format!("MATCH (p:person) WHERE {pred} RETURN count(*) AS n");
        assert!(compiled(&mut session, &source), "{what} in a filter");
        assert_eq!(rows(&mut session, &source), on_rows(&mut session, &source));
    }
}

/// And the register of what is left, which is read as a list or a
/// path. Asserting the fallback rather than only writing it down is
/// what makes this a line that fails when the gap closes, so the day a
/// list vector lands this test says so.
#[test]
fn what_has_no_kernel_is_a_list_or_a_path() {
    let (_dir, mut session) = opened("kernels-uncovered.zu1", SAFE);
    for (what, source, why) in UNCOVERED {
        assert!(
            !compiled(&mut session, source),
            "{what} compiled, so the register is out of date: {why}"
        );
        assert_eq!(rows(&mut session, source).len(), 4, "{what} still answers");
    }
}

/// A function that has no answer for a row raises the same condition
/// on both engines, with the same code. This is the reason a kernel
/// over a whole morsel is not simply the faster way to do the same
/// thing: it measures rows the row executor would have reached one at
/// a time, so it has to reach the same conditions in the same cases.
#[test]
fn a_row_with_no_answer_raises_the_same_condition_on_both_engines() {
    let source = "MATCH (p:person) RETURN ln(p.height) AS v";
    let (_safe_dir, mut safe) = opened("kernels-raise-safe.zu1", SAFE);
    assert!(compiled(&mut safe, source), "the plan is the pipeline");
    let (_dir, mut session) = opened("kernels-raise.zu1", RAISES);
    assert_eq!(status(&mut session, source), "2201E");
    session.set_options(Options {
        engine: Engine::Rows,
        ..session.options().clone()
    });
    assert_eq!(status(&mut session, source), "2201E");
}

/// The one rule that keeps a can-raise function off the pipeline: a
/// computed column is filled where the level is built, so anything
/// that drops a row after that would have the kernel measuring a row
/// the query said it would not. A guard in front of the function is
/// exactly that, and the plan goes back whole rather than raising a
/// condition the query ruled out.
#[test]
fn a_guard_in_front_of_the_function_keeps_the_plan_on_the_row_executor() {
    let (_dir, mut session) = opened("kernels-guard.zu1", RAISES);
    let source = "MATCH (p:person) WHERE p.height > 0 RETURN ln(p.height) AS v ORDER BY v";
    assert!(
        !compiled(&mut session, source),
        "the guard is what the fallback is for"
    );
    let answer = rows(&mut session, source);
    assert_eq!(answer.len(), 3, "the row with no answer is not one of them");
    assert_eq!(answer, on_rows(&mut session, source));
}

/// The same rule for a slice, from the other side. The row executor
/// fills the projection for every row the match produced and slices
/// the answer afterwards, so it raises whatever the limit said; the
/// pipeline stops reading once it holds the rows the limit asked for,
/// so a limit satisfied inside the first morsel would never reach the
/// row with no answer. That is a disagreement in the direction nobody
/// notices, which is the worse kind, and the plan going back is what
/// keeps it from happening.
#[test]
fn a_slice_over_the_answer_keeps_the_plan_on_the_row_executor() {
    let (_safe_dir, mut safe) = opened("kernels-slice-safe.zu1", SAFE);
    let source = "MATCH (p:person) RETURN ln(p.height) AS v LIMIT 1";
    assert!(!compiled(&mut safe, source), "a limit stops partway");
    let (_dir, mut session) = opened("kernels-slice.zu1", RAISES);
    assert_eq!(status(&mut session, source), "2201E");
    session.set_options(Options {
        engine: Engine::Rows,
        ..session.options().clone()
    });
    assert_eq!(status(&mut session, source), "2201E");
}

/// A function that cannot raise is a kernel wherever it is written,
/// guard or no guard, which is what the rule costs and no more.
#[test]
fn a_function_that_cannot_raise_is_a_kernel_behind_a_guard_too() {
    let (_dir, mut session) = opened("kernels-safe.zu1", RAISES);
    let source = "MATCH (p:person) WHERE p.height > 0 RETURN floor(p.height) AS v ORDER BY v";
    assert!(compiled(&mut session, source), "floor() has every answer");
    assert_eq!(rows(&mut session, source), on_rows(&mut session, source));
}
