//! The character string value expressions of ISO 20.22 to 20.24.
//!
//! Two strings are joined with an operator and asked five questions by
//! name. What is only checkable here is that the text reaches the
//! executor and comes back over stored values as well as over literals:
//! the operator binds where the standard puts it, the two lengths part
//! company on anything that is not ASCII, and a number written where a
//! string belongs is refused rather than measured by its spelling.

use zu::Database;
use zu::query::Value;

/// Three names with spaces around one of them, and one word that is not
/// ASCII, which is the only thing that tells the two lengths apart.
fn people(dir: &std::path::Path) -> Database {
    let db = Database::create(dir.join("strings.zu1")).expect("create");
    {
        let mut conn = db.connect().expect("connect");
        for (tag, name) in [("plain", "ana"), ("spaced", "  bo  "), ("wide", "日本")] {
            conn.execute(&format!(
                "INSERT (p:person {{tag: '{tag}', name: '{name}'}})"
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

fn str_of(db: &Database, source: &str) -> String {
    match one(db, source) {
        Value::Str(s) => s,
        other => panic!("{source} answered {other:?}, not a string"),
    }
}

fn int_of(db: &Database, source: &str) -> i64 {
    match one(db, source) {
        Value::Int(n) => n,
        other => panic!("{source} answered {other:?}, not a number"),
    }
}

fn refused(db: &Database, source: &str) -> String {
    let mut conn = db.connect().expect("connect");
    conn.query(source)
        .expect_err(&format!("{source} should have been refused"))
        .to_string()
}

#[test]
fn two_strings_are_joined_and_the_join_folds_to_the_left() {
    let dir = tempfile::tempdir().unwrap();
    let db = people(dir.path());
    assert_eq!(str_of(&db, "RETURN 'ab' || 'cd' AS v"), "abcd");
    assert_eq!(str_of(&db, "RETURN 'a' || 'b' || 'c' AS v"), "abc");
    // And it reads a stored value the same way it reads a literal.
    assert_eq!(
        str_of(
            &db,
            "MATCH (p:person) WHERE p.tag = 'plain' RETURN 'mr ' || p.name AS v"
        ),
        "mr ana"
    );
}

#[test]
fn the_join_binds_tighter_than_a_comparison_and_looser_than_a_sum() {
    let dir = tempfile::tempdir().unwrap();
    let db = people(dir.path());
    // The comparison is asked about the joined string, so this is one
    // equality rather than a join of a string and a truth value.
    assert_eq!(
        one(&db, "RETURN 'ab' || 'cd' = 'abcd' AS v"),
        Value::Bool(true)
    );
    // The sum happens first, so what is joined is the three and not the
    // one, and a reader who wanted the digits joined writes brackets.
    assert_eq!(
        str_of(&db, "RETURN 'n=' || CAST(1 + 2 AS STRING) AS v"),
        "n=3"
    );
}

#[test]
fn a_join_with_a_null_anywhere_in_it_is_null() {
    let dir = tempfile::tempdir().unwrap();
    let db = people(dir.path());
    assert_eq!(one(&db, "RETURN 'ab' || NULL AS v"), Value::Null);
    assert_eq!(one(&db, "RETURN NULL || 'ab' AS v"), Value::Null);
}

#[test]
fn the_characters_of_a_string_and_the_bytes_of_it() {
    let dir = tempfile::tempdir().unwrap();
    let db = people(dir.path());
    assert_eq!(int_of(&db, "RETURN CHAR_LENGTH('abcd') AS n"), 4);
    // The spelled out name is the same function and not another one.
    assert_eq!(int_of(&db, "RETURN CHARACTER_LENGTH('abcd') AS n"), 4);
    assert_eq!(int_of(&db, "RETURN OCTET_LENGTH('abcd') AS n"), 4);
    // Two characters the store keeps in six bytes, which is where the
    // two questions part company and why both of them are here.
    let chars = "MATCH (p:person) WHERE p.tag = 'wide' RETURN CHAR_LENGTH(p.name) AS n";
    let bytes = "MATCH (p:person) WHERE p.tag = 'wide' RETURN OCTET_LENGTH(p.name) AS n";
    assert_eq!(int_of(&db, chars), 2);
    assert_eq!(int_of(&db, bytes), 6);
}

#[test]
fn a_string_folds_up_and_down_and_loses_the_spaces_at_its_ends() {
    let dir = tempfile::tempdir().unwrap();
    let db = people(dir.path());
    assert_eq!(str_of(&db, "RETURN UPPER('aB') AS v"), "AB");
    assert_eq!(str_of(&db, "RETURN LOWER('aB') AS v"), "ab");
    assert_eq!(str_of(&db, "RETURN TRIM('  ab  ') AS v"), "ab");
    // TRIM takes the spaces off both ends and leaves the one in the
    // middle, which is the difference between trimming and stripping.
    assert_eq!(
        str_of(
            &db,
            "MATCH (p:person) WHERE p.tag = 'spaced' RETURN UPPER(TRIM(p.name)) AS v"
        ),
        "BO"
    );
}

#[test]
fn a_string_function_over_a_null_answers_null() {
    let dir = tempfile::tempdir().unwrap();
    let db = people(dir.path());
    for source in [
        "RETURN CHAR_LENGTH(NULL) AS v",
        "RETURN OCTET_LENGTH(NULL) AS v",
        "RETURN UPPER(NULL) AS v",
        "RETURN LOWER(NULL) AS v",
        "RETURN TRIM(NULL) AS v",
    ] {
        assert_eq!(one(&db, source), Value::Null, "{source}");
    }
}

#[test]
fn a_number_written_where_a_string_belongs_is_refused_by_name() {
    let dir = tempfile::tempdir().unwrap();
    let db = people(dir.path());
    // Nothing casts itself to a string on the way in, so a query that
    // meant the digits of the number says so with a CAST and one that
    // meant a sum wrote a plus. Both are told which it was.
    let err = refused(&db, "RETURN 'ab' || 1 AS v");
    assert!(err.contains("joins strings"), "{err}");
    for source in [
        "RETURN CHAR_LENGTH(1) AS v",
        "RETURN UPPER(1) AS v",
        "RETURN TRIM(1) AS v",
    ] {
        let err = refused(&db, source);
        assert!(err.contains("needs a string"), "{source}: {err}");
    }
}
