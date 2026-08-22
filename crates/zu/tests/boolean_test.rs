//! The boolean test, ISO 20.20's `<boolean test>`.
//!
//! `x IS TRUE` is the one construct in GQL that answers a two valued
//! question about a three valued expression. Every other boolean
//! operator carries unknown through: `NULL AND TRUE` is unknown, `NOT
//! NULL` is unknown, and a WHERE handed unknown drops the row without
//! ever saying which of the two reasons it had. The test is where a
//! query gets to ask, and the whole of what these cases measure is
//! that it never answers unknown itself.
//!
//! The negated spelling is an exact complement rather than a third
//! answer, because there is no third answer left for it to fall into:
//! `x IS NOT TRUE` is true whenever `x IS TRUE` is false, which for a
//! null operand means both `IS NOT TRUE` and `IS NOT FALSE` are true
//! at once. That looks wrong until you read it as what it says, which
//! is that a value nobody knows is not known to be either one.

use zu::query::{Value, run};
use zu::{Database, Engine, Options};
use zu_zu1::file::Zu1File;

fn db(dir: &std::path::Path) -> Zu1File {
    Zu1File::create(&dir.join("boolean_test.zu1")).unwrap()
}

fn one(db: &mut Zu1File, source: &str) -> Value {
    let result = run(source, db, &[]).unwrap_or_else(|e| panic!("{source}: {e}"));
    assert_eq!(result.rows.len(), 1, "{source} returned {:?}", result.rows);
    result.rows[0][0].clone()
}

/// Whether the expression came back true, refusing anything that is
/// not a truth value at all. Every case here goes through this, so a
/// test that answered null would fail on the shape before it failed on
/// the value.
fn yes(db: &mut Zu1File, source: &str) -> bool {
    match one(db, &format!("RETURN {source} AS v")) {
        Value::Bool(b) => b,
        other => panic!("{source} is {other:?}, not a truth value"),
    }
}

/// The three operands the test can be handed, written as expressions
/// so none of them is a literal the binder could fold differently.
const TRUTHS: [&str; 3] = ["(1 = 1)", "(1 = 2)", "(1 = NULL)"];

#[test]
fn the_test_answers_which_of_the_three_a_value_is() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = db(dir.path());
    assert!(yes(&mut db, "(1 = 1) IS TRUE"));
    assert!(!yes(&mut db, "(1 = 2) IS TRUE"));
    assert!(!yes(&mut db, "(1 = NULL) IS TRUE"));

    assert!(!yes(&mut db, "(1 = 1) IS FALSE"));
    assert!(yes(&mut db, "(1 = 2) IS FALSE"));
    assert!(!yes(&mut db, "(1 = NULL) IS FALSE"));

    assert!(!yes(&mut db, "(1 = 1) IS UNKNOWN"));
    assert!(!yes(&mut db, "(1 = 2) IS UNKNOWN"));
    assert!(yes(&mut db, "(1 = NULL) IS UNKNOWN"));
}

#[test]
fn the_negated_spelling_is_the_complement_and_not_a_third_answer() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = db(dir.path());
    for operand in TRUTHS {
        for word in ["TRUE", "FALSE", "UNKNOWN"] {
            let plain = yes(&mut db, &format!("{operand} IS {word}"));
            let negated = yes(&mut db, &format!("{operand} IS NOT {word}"));
            assert_ne!(plain, negated, "{operand} IS [NOT] {word}");
        }
    }
    // The reading that surprises people, spelled out: an unknown
    // operand is not known to be true and not known to be false, so
    // both negated tests hold at once.
    assert!(yes(&mut db, "(1 = NULL) IS NOT TRUE"));
    assert!(yes(&mut db, "(1 = NULL) IS NOT FALSE"));
}

#[test]
fn no_operand_makes_the_test_answer_unknown() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = db(dir.path());
    for operand in TRUTHS {
        for word in ["TRUE", "FALSE", "UNKNOWN"] {
            for not in ["", "NOT "] {
                let source = format!("{operand} IS {not}{word}");
                // `yes` panics on anything that is not a boolean, so
                // reaching the assertion is the check.
                let _ = yes(&mut db, &source);
            }
        }
    }
}

/// ISO writes the test over a `<boolean primary>`, which puts it below
/// the comparison: `1 = 1 IS TRUE` is `(1 = 1) IS TRUE` and not `1 =
/// (1 IS TRUE)`. The two group the same value here, so what pins the
/// reading down is a comparison whose right side is not a boolean at
/// all: if the test bound to the `1` on the right, the equality would
/// be between a number and a truth value.
#[test]
fn the_test_binds_looser_than_the_comparison() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = db(dir.path());
    assert!(yes(&mut db, "1 = 1 IS TRUE"));
    assert!(yes(&mut db, "1 = 2 IS FALSE"));
    assert!(yes(&mut db, "1 = NULL IS UNKNOWN"));
    assert!(yes(&mut db, "'a' = 'a' IS TRUE"));
}

/// A test answers a boolean, so another test may be written after it,
/// and the chain reads left to right the way the standard's recursion
/// does.
#[test]
fn one_test_may_be_written_after_another() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = db(dir.path());
    assert!(yes(&mut db, "(1 = NULL) IS UNKNOWN IS TRUE"));
    assert!(yes(&mut db, "(1 = NULL) IS TRUE IS FALSE"));
    assert!(yes(&mut db, "(1 = 1) IS TRUE IS NOT UNKNOWN"));
}

/// The word UNKNOWN is the boolean type's null (ISO 21.2), so it is
/// written where a literal is written and behaves as one everywhere
/// the null does.
#[test]
fn unknown_is_written_as_a_literal() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = db(dir.path());
    assert_eq!(one(&mut db, "RETURN UNKNOWN AS v"), Value::Null);
    assert!(yes(&mut db, "UNKNOWN IS UNKNOWN"));
    assert!(yes(&mut db, "NULL IS UNKNOWN"));
    assert!(!yes(&mut db, "(UNKNOWN AND TRUE) IS FALSE"));
    assert!(yes(&mut db, "(UNKNOWN AND TRUE) IS UNKNOWN"));
    assert!(yes(&mut db, "(UNKNOWN OR TRUE) IS TRUE"));
}

/// The operand has to be a boolean. IS NULL takes anything, because
/// whether there is a value is a question about every value, but
/// whether a string is TRUE is not a question with an answer.
#[test]
fn a_non_boolean_operand_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = db(dir.path());
    let err = run("RETURN 'a' IS TRUE AS v", &mut db, &[])
        .unwrap_err()
        .to_string();
    assert!(err.contains("IS TRUE needs a boolean"), "{err}");
    let err = run("RETURN 1 IS NOT FALSE AS v", &mut db, &[])
        .unwrap_err()
        .to_string();
    assert!(err.contains("IS FALSE needs a boolean"), "{err}");
}

/// The word after IS still has to be one of the four, and the message
/// says which four rather than naming only NULL.
#[test]
fn a_word_that_is_not_a_truth_value_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = db(dir.path());
    let err = run("RETURN (1 = 1) IS MAYBE AS v", &mut db, &[])
        .unwrap_err()
        .to_string();
    assert!(err.contains("NULL, TRUE, FALSE, or UNKNOWN"), "{err}");
}

/// A test in a WHERE is the reason the construct exists: it turns the
/// rows a three valued predicate dropped silently into rows a query
/// can name.
#[test]
fn a_where_can_ask_for_the_rows_the_predicate_was_unsure_about() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = db(dir.path());
    let rows = run(
        "UNWIND [1, 2, NULL] AS n WITH n WHERE (n > 1) IS NOT TRUE RETURN n",
        &mut db,
        &[],
    )
    .expect("query")
    .rows;
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert_eq!(rows[0][0], Value::Int(1));
    assert_eq!(rows[1][0], Value::Null);
}

/// Three people, one of whom has no age, which is the column a stored
/// null lives in.
fn people(dir: &std::path::Path) -> Database {
    let db = Database::create(dir.join("people.zu1")).expect("create");
    {
        let mut conn = db.connect().expect("connect");
        conn.execute("INSERT (p:person {uid: 1, name: 'ada', age: 36})")
            .expect("ada");
        conn.execute("INSERT (p:person {uid: 2, name: 'bo', age: 20})")
            .expect("bo");
        conn.execute("INSERT (p:person {uid: 3, name: 'cy', age: NULL})")
            .expect("cy");
    }
    db
}

/// A test over a stored column, which is the shape the vector engine
/// has an opinion about. It has no op for this one, so the filter goes
/// back to the row engine, and what matters is that it goes back
/// rather than answering differently: the two engines are asked the
/// same question here and have to give the same names.
#[test]
fn a_test_over_a_stored_column_answers_the_same_in_both_engines() {
    let dir = tempfile::tempdir().unwrap();
    let db = people(dir.path());
    let source = "MATCH (p:person) WHERE (p.age > 30) IS NOT TRUE \
                  RETURN p.name AS name ORDER BY name";
    let names = |engine: Engine| -> Vec<String> {
        let mut conn = db.connect().expect("connect");
        let options = Options {
            engine,
            ..conn.session_mut().options().clone()
        };
        conn.session_mut().set_options(options);
        conn.query(source)
            .unwrap_or_else(|e| panic!("{source}: {e}"))
            .iter()
            .map(|row| row.get_by_name::<String>("name").expect("a name"))
            .collect()
    };
    let names = {
        let pipeline = names(Engine::Pipeline);
        assert_eq!(pipeline, names(Engine::Rows), "the two engines disagreed");
        pipeline
    };
    // bo is under thirty and cy has no age at all, so the predicate is
    // false for one and unknown for the other, and IS NOT TRUE is the
    // only spelling that keeps them both.
    assert_eq!(names, vec!["bo".to_string(), "cy".to_string()]);
}
