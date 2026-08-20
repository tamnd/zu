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
        for (name, height) in [("ana", -2.5), ("bo", 6.25)] {
            conn.execute(&format!(
                "INSERT (p:person {{name: '{name}', height: {height}}})"
            ))
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

/// GF01 to GF03, the numeric library over a column: what the kernels
/// answer per row, that an exact argument stays exact through the
/// roundings, and that a function with no answer for the value one row
/// holds raises the condition the standard names rather than handing
/// back a NaN that every comparison below would read as false.
#[test]
fn the_numeric_library_answers_over_a_column() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = opened(dir.path());
    let mut conn = db.connect().expect("connect");

    let source = "MATCH (p:person) RETURN ABS(p.height) AS a, FLOOR(p.height) AS f, SQRT(ABS(p.height)) AS s ORDER BY a";
    let rows = conn
        .query(source)
        .unwrap_or_else(|e| panic!("{source}: {e}"));
    let read = |rows: &zu::query::QueryResult, name: &str| -> Vec<f64> {
        rows.iter()
            .map(|row| row.get_by_name::<f64>(name).expect(name))
            .collect()
    };
    assert_eq!(read(&rows, "a"), [2.5, 6.25]);
    assert_eq!(read(&rows, "f"), [-3.0, 6.0]);
    assert_eq!(read(&rows, "s"), [2.5_f64.sqrt(), 2.5]);

    // The call over a column stays in the plan, one literal argument
    // and all is answered while binding, and the two agree.
    let plan = conn
        .explain("MATCH (p:person) RETURN ABS(p.height) AS a")
        .expect("explain");
    assert!(plan.contains("abs("), "{plan}");
    let plan = conn.explain("RETURN ABS(-3) AS a").expect("explain");
    assert!(!plan.contains("abs("), "{plan}");
    assert_eq!(one(&db, "RETURN ABS(-3) AS a"), Value::Int(3));
    assert_eq!(one(&db, "RETURN MOD(7, 3) AS m"), Value::Int(1));
    assert_eq!(one(&db, "RETURN POWER(2, 3) AS p"), Value::Float(8.0));

    // A condition the values raise is raised where the values are, so
    // the same statement over a column that held no negative height
    // would have answered.
    let says = refused(&db, "MATCH (p:person) RETURN LN(p.height) AS l");
    assert!(says.contains("ln()"), "{says}");
    let says = refused(&db, "RETURN MOD(1, 0) AS m");
    assert!(says.contains("division by zero"), "{says}");
}

/// GF05 and GF06, the trim family: the explicit form over a column,
/// which is the one spelling that is not written like a call, and the
/// three multi-character functions beside it.
#[test]
fn the_trim_family_answers_over_a_column() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = opened(dir.path());
    let mut conn = db.connect().expect("connect");

    // The names in the fixture have no spaces on them, so what is
    // trimmed here is a letter, which is also the only way to see that
    // the end named is the end trimmed.
    let source = "MATCH (p:person) RETURN TRIM(LEADING 'a' FROM p.name) AS l ORDER BY l";
    let rows = conn
        .query(source)
        .unwrap_or_else(|e| panic!("{source}: {e}"));
    let names: Vec<String> = rows
        .iter()
        .map(|row| row.get_by_name::<String>("l").expect("l"))
        .collect();
    assert_eq!(names, ["bo", "na"]);

    // The call over a column stays in the plan and is printed under the
    // name of the row that answers it, and the same form over what the
    // statement wrote is answered while binding.
    let plan = conn.explain(source).expect("explain");
    assert!(plan.contains("trim_leading("), "{plan}");
    let plan = conn
        .explain("RETURN TRIM(LEADING 'x' FROM 'xxay') AS v")
        .expect("explain");
    assert!(!plan.contains("trim"), "{plan}");

    assert_eq!(
        one(&db, "RETURN TRIM(TRAILING 'y' FROM 'xxay') AS v"),
        Value::Str("xxa".into())
    );
    assert_eq!(
        one(&db, "RETURN TRIM('  a  ') AS v"),
        Value::Str("a".into())
    );
    assert_eq!(
        one(&db, "RETURN BTRIM('xyaxy', 'xy') AS v"),
        Value::Str("a".into())
    );

    // The three words are ordinary names everywhere else, which is what
    // keeps a query that bound one of them readable.
    assert_eq!(
        one(&db, "LET leading = 'x' RETURN TRIM(leading) AS v"),
        Value::Str("x".into())
    );

    // One character, and the condition the standard names when it is
    // handed more, which is the whole reason the three above exist.
    let says = refused(&db, "RETURN TRIM(BOTH 'ab' FROM 'abx') AS v");
    assert!(says.contains("trims one character"), "{says}");
}

/// ISO 20.24, the substring function: LEFT and RIGHT over a column,
/// which in GQL are the whole of it, since SUBSTRING is a word the
/// standard has reserved and given no meaning to.
#[test]
fn the_substring_function_answers_over_a_column() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = opened(dir.path());
    let mut conn = db.connect().expect("connect");

    let source = "MATCH (p:person) RETURN LEFT(p.name, 2) AS l ORDER BY l";
    let rows = conn
        .query(source)
        .unwrap_or_else(|e| panic!("{source}: {e}"));
    let names: Vec<String> = rows
        .iter()
        .map(|row| row.get_by_name::<String>("l").expect("l"))
        .collect();
    assert_eq!(names, ["an", "bo"]);

    let plan = conn.explain(source).expect("explain");
    assert!(plan.contains("left("), "{plan}");
    let plan = conn.explain("RETURN LEFT('abc', 2) AS l").expect("explain");
    assert!(!plan.contains("left("), "{plan}");

    assert_eq!(
        one(&db, "RETURN RIGHT('abc', 2) AS v"),
        Value::Str("bc".into())
    );
    // The middle of a string is one written inside the other, there
    // being no third function for it.
    assert_eq!(
        one(&db, "RETURN LEFT(RIGHT('abcde', 4), 2) AS v"),
        Value::Str("bc".into())
    );

    // A count is a number and a string is a string, both settled while
    // binding, and a count no string has is the standard's condition.
    let says = refused(&db, "RETURN LEFT('abc', 'two') AS v");
    assert!(
        says.contains("left() needs a string and a count of characters"),
        "{says}"
    );
    let says = refused(&db, "MATCH (p:person) RETURN RIGHT(p.name, -1) AS v");
    assert!(says.contains("negative number"), "{says}");
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

    let says = refused(&db, "RETURN ABS('a') AS v");
    assert!(says.contains("abs() needs a number"), "{says}");

    let says = refused(&db, "RETURN ROUND(1.5, 2, 3) AS v");
    assert!(says.contains("takes 1 or 2 argument(s), got 3"), "{says}");
}
