//! The function registry of ISO 20: what a builtin is called, what it
//! takes, what it answers, and when it is answered.
//!
//! One table holds all four, so what these tests ask is that the table
//! is the only place the answers come from. A name resolves to a
//! signature and every spelling of that name resolves to the same one,
//! a call that does not fit the signature is refused by the signature's
//! own words, and a deterministic call over what the statement wrote is
//! answered while the statement is bound rather than once per row it
//! would have reached.

use zu::Database;
use zu::query::Value;

fn opened(dir: &std::path::Path) -> Database {
    let db = Database::create(dir.join("functions.zu1")).expect("create");
    {
        let mut conn = db.connect().expect("connect");
        for name in ["ana", "bo"] {
            conn.execute(&format!("INSERT (p:person {{name: '{name}'}})"))
                .expect("a person");
        }
    }
    db
}

fn one(db: &Database, source: &str) -> Value {
    let mut conn = db.connect().expect("connect");
    let rows = conn
        .query(source)
        .unwrap_or_else(|e| panic!("{source}: {e}"));
    let rows: Vec<_> = rows.iter().collect();
    assert_eq!(rows.len(), 1, "{source} answered {} rows", rows.len());
    rows[0].value(0).expect("a value").clone()
}

fn refused(db: &Database, source: &str) -> String {
    let mut conn = db.connect().expect("connect");
    conn.query(source)
        .expect_err(&format!("{source} should have been refused"))
        .to_string()
}

/// A name is matched against the table without regard to case, and the
/// long spelling of a function is the same function as the short one,
/// so the two answer the same value and the plan names them alike.
#[test]
fn every_spelling_of_a_name_reaches_the_same_function() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = opened(dir.path());
    let mut conn = db.connect().expect("connect");

    for source in [
        "MATCH (p:person) RETURN CHAR_LENGTH(p.name) AS n ORDER BY n",
        "MATCH (p:person) RETURN character_length(p.name) AS n ORDER BY n",
        "MATCH (p:person) RETURN Char_Length(p.name) AS n ORDER BY n",
    ] {
        let rows = conn
            .query(source)
            .unwrap_or_else(|e| panic!("{source}: {e}"));
        let lengths: Vec<i64> = rows
            .iter()
            .map(|row| row.get_by_name::<i64>("n").expect("n"))
            .collect();
        assert_eq!(lengths, [2, 3], "{source}");
        let plan = conn.explain(source).expect("explain");
        assert!(plan.contains("char_length("), "{source}: {plan}");
    }
}

/// A deterministic function over values the statement wrote answers the
/// same thing on every row, so it is answered once while binding and
/// the plan carries the answer instead of the call.
#[test]
fn a_call_over_what_the_statement_wrote_is_answered_once() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = opened(dir.path());
    let mut conn = db.connect().expect("connect");

    let plan = conn
        .explain("MATCH (p:person) RETURN UPPER('ab') AS u")
        .expect("explain");
    assert!(plan.contains("'AB'"), "{plan}");
    assert!(!plan.contains("upper("), "{plan}");

    // And the answer is the answer, folded or not.
    assert_eq!(one(&db, "RETURN UPPER('ab') AS u"), Value::Str("AB".into()));
    assert_eq!(one(&db, "RETURN CHAR_LENGTH('日本') AS n"), Value::Int(2));

    // A call over a column is a call: the plan keeps it, because what
    // it answers is one thing per row.
    let plan = conn
        .explain("MATCH (p:person) RETURN UPPER(p.name) AS u")
        .expect("explain");
    assert!(plan.contains("upper("), "{plan}");
}

/// What a signature refuses, and in its own words: a name no builtin
/// has, a count of arguments the signature does not allow, and a type
/// the function has nothing to say about.
#[test]
fn what_the_registry_refuses() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = opened(dir.path());

    let says = refused(&db, "RETURN NOSUCHTHING(1) AS v");
    assert!(says.contains("unknown function"), "{says}");

    let says = refused(&db, "RETURN CHAR_LENGTH('a', 'b') AS v");
    assert!(says.contains("takes 1 argument(s), got 2"), "{says}");

    let says = refused(&db, "MATCH (p:person) RETURN SAME(p) AS v");
    assert!(says.contains("at least two"), "{says}");

    let says = refused(&db, "RETURN CHAR_LENGTH(1) AS v");
    assert!(says.contains("char_length() needs a string"), "{says}");

    let says = refused(&db, "RETURN CARDINALITY('ab') AS v");
    assert!(says.contains("cardinality() needs a list"), "{says}");

    let says = refused(&db, "MATCH (p:person) RETURN UPPER(*) AS v");
    assert!(says.contains("only count(*) takes *"), "{says}");
}
