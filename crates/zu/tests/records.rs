//! Record values: building one, asking what it is, and casting it.
//!
//! The record *type* has been parseable since the lattice went in, so
//! `IS TYPED RECORD` answered a question about a value that could not
//! exist. What is new is the value, and the value is what the three
//! conditions ISO names for records are about: two records that cannot
//! be compared, a field the declared type wants and the record lacks,
//! and a field that is there and will not fit. All three are checked
//! here end to end, because the interesting mistakes are the ones a
//! reasonable engine makes quietly: answering false where the standard
//! says raise, and answering about the wrong field.

use zu::query::{Value, run};
use zu_zu1::file::Zu1File;
use zu_zu1::graph::bulk_load_as;

fn graph(dir: &std::path::Path) -> Zu1File {
    let mut zu = Zu1File::create(&dir.join("records.zu1")).unwrap();
    bulk_load_as(&mut zu, "person", "knows", 2, &[(0, 1)]).unwrap();
    zu
}

fn one(db: &mut Zu1File, source: &str) -> Value {
    let result = run(source, db, &[]).unwrap_or_else(|e| panic!("{source}: {e}"));
    assert_eq!(result.rows.len(), 1, "{source} returned {:?}", result.rows);
    result.rows[0][0].clone()
}

fn yes(db: &mut Zu1File, predicate: &str) -> bool {
    match one(db, &format!("RETURN ({predicate}) AS v")) {
        Value::Bool(b) => b,
        other => panic!("{predicate} answered {other:?}"),
    }
}

fn code(db: &mut Zu1File, source: &str) -> String {
    let err = run(source, db, &[]).expect_err(source);
    err.gqlstatus()
        .unwrap_or_else(|| panic!("{source}: {err} carries no status"))
        .code()
        .to_string()
}

fn record(fields: &[(&str, Value)]) -> Value {
    Value::record(
        fields
            .iter()
            .map(|(n, v)| ((*n).to_owned(), v.clone()))
            .collect(),
    )
}

/// A record is its fields, and the order they were written in is not
/// one of them. Two records with the same fields are one value however
/// the query spelled them, which is the rule that makes field order a
/// spelling rather than data.
#[test]
fn a_record_is_its_fields_and_not_the_order_they_were_written_in() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = graph(dir.path());
    assert_eq!(
        one(&mut db, "RETURN {b: 'x', a: 1} AS v"),
        record(&[("a", Value::Int(1)), ("b", Value::Str("x".into()))])
    );
    assert!(yes(&mut db, "{a: 1, b: 2} = {b: 2, a: 1}"));
    assert!(!yes(&mut db, "{a: 1, b: 2} = {a: 1, b: 3}"));
    assert_eq!(one(&mut db, "RETURN {} AS v"), record(&[]));

    // A field written twice is a typo every time. Neither rule for
    // resolving it, first wins or last wins, is what the query meant.
    assert_eq!(code(&mut db, "RETURN {a: 1, a: 2} AS v"), "42001");
}

/// 22G0U. Two records are comparable when they name the same fields,
/// and when they do not there is no field by field comparison to make.
/// False is the wrong answer and wrong in the way that hides: it is
/// also the answer for two records that differ in a value, so a query
/// that misspelled a field name would read the mistake as data.
#[test]
fn records_with_different_fields_cannot_be_compared() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = graph(dir.path());
    for source in [
        "RETURN ({a: 1} = {b: 1}) AS v",
        "RETURN ({a: 1} <> {b: 1}) AS v",
        "RETURN ({a: 1, b: 2} = {a: 1}) AS v",
        // A record inside a list is compared the same way, so the
        // condition has to survive the recursion rather than be lost
        // to the list's own answer.
        "RETURN ([{a: 1}] = [{b: 1}]) AS v",
    ] {
        assert_eq!(code(&mut db, source), "22G0U", "{source}");
    }

    // Null is null before it is anything else: a comparison involving
    // one has no fields to disagree about.
    assert_eq!(one(&mut db, "RETURN ({a: 1} = null) AS v"), Value::Null);
}

/// Membership is not conversion. An open record type says only what a
/// record has at least, a closed one says what it has, and neither
/// changes the value or fails: the predicate answers.
#[test]
fn a_record_type_says_which_records_belong_to_it() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = graph(dir.path());
    // A bare RECORD and ANY RECORD are the same open type, GV47.
    assert!(yes(&mut db, "{a: 1, b: 2} IS TYPED RECORD"));
    assert!(yes(&mut db, "{a: 1, b: 2} IS TYPED ANY RECORD"));
    assert!(yes(&mut db, "{} IS TYPED RECORD"));

    // GV46, the closed type: every field it names, present, of that
    // type, and nothing else.
    assert!(yes(&mut db, "{a: 1} IS TYPED RECORD { a :: INT }"));
    assert!(!yes(&mut db, "{a: 'x'} IS TYPED RECORD { a :: INT }"));
    assert!(!yes(&mut db, "{a: 1, b: 2} IS TYPED RECORD { a :: INT }"));
    assert!(!yes(&mut db, "{} IS TYPED RECORD { a :: INT }"));

    // A field's type is an ordinary value type, so it is nullable
    // unless it says otherwise and it nests, which is GV48.
    assert!(yes(&mut db, "{a: null} IS TYPED RECORD { a :: INT }"));
    assert!(!yes(
        &mut db,
        "{a: null} IS TYPED RECORD { a :: INT NOT NULL }"
    ));
    assert!(yes(
        &mut db,
        "{a: {b: 1}} IS TYPED RECORD { a :: RECORD { b :: INT } }"
    ));

    // Nothing else is a record, and a record is nothing else.
    assert!(!yes(&mut db, "1 IS TYPED RECORD"));
    assert!(!yes(&mut db, "[1] IS TYPED RECORD"));
    assert!(!yes(&mut db, "{a: 1} IS TYPED INT"));
    assert!(!yes(&mut db, "{a: 1} IS TYPED LIST<ANY>"));
}

/// The cast is fieldwise and it fails in the two ways ISO names
/// separately: 22G0Y is a field the type declares and the record does
/// not carry, which is a fact about the record's shape, and 22G0X is a
/// field that is there and will not go into its declared type, which
/// is a fact about that field's value and says which field.
#[test]
fn casting_to_a_record_type_casts_the_fields_and_names_the_one_that_fails() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = graph(dir.path());
    assert_eq!(
        one(
            &mut db,
            "RETURN CAST({a: '1', b: 2} AS RECORD { a :: INT, b :: STRING }) AS v"
        ),
        record(&[("a", Value::Int(1)), ("b", Value::Str("2".into()))])
    );
    assert_eq!(
        code(
            &mut db,
            "RETURN CAST({a: 1} AS RECORD { a :: INT, b :: INT }) AS v"
        ),
        "22G0Y"
    );
    assert_eq!(
        code(&mut db, "RETURN CAST({a: 'x'} AS RECORD { a :: INT }) AS v"),
        "22G0X"
    );
    // The failing field is named, and so is the condition underneath.
    let err = run(
        "RETURN CAST({a: 1, b: 'x'} AS RECORD { a :: INT, b :: INT }) AS v",
        &mut db,
        &[],
    )
    .expect_err("a field that does not cast");
    let message = err.to_string();
    assert!(message.contains("'b'"), "{message}");
    assert!(message.contains("22018"), "{message}");

    // A closed record type says what the record has, so a field it
    // does not name is dropped rather than carried; an open one says
    // only what the record has at least, so the extra field survives.
    assert_eq!(
        one(
            &mut db,
            "RETURN CAST({a: 1, b: 2} AS RECORD { a :: INT }) AS v"
        ),
        record(&[("a", Value::Int(1))])
    );
    assert_eq!(
        one(&mut db, "RETURN CAST({a: 1, b: 2} AS ANY RECORD) AS v"),
        record(&[("a", Value::Int(1)), ("b", Value::Int(2))])
    );

    // Only a record casts to a record. Wrapping a scalar in a record
    // of one would turn a mistyped query into a quiet success, the
    // same way wrapping one in a list of one would.
    assert_eq!(
        code(&mut db, "RETURN CAST(1 AS RECORD { a :: INT }) AS v"),
        "22018"
    );
    assert_eq!(
        one(&mut db, "RETURN CAST(null AS RECORD { a :: INT }) AS v"),
        Value::Null
    );
}

/// A field is read with the same dot a property is, and a field the
/// record does not carry is null rather than an error, which is what a
/// property a node does not carry already answers. A query that needs
/// the shape guaranteed asks for it with a cast.
#[test]
fn a_field_is_read_with_a_dot_and_a_missing_one_is_null() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = graph(dir.path());
    assert_eq!(one(&mut db, "RETURN {a: 1}.a AS v"), Value::Int(1));
    assert_eq!(one(&mut db, "RETURN {a: {b: 2}}.a.b AS v"), Value::Int(2));
    assert_eq!(one(&mut db, "RETURN {a: 1}.zzz AS v"), Value::Null);

    // A record holds whatever an expression evaluates to, including a
    // node, and reading the field back gives the node itself rather
    // than something flattened on the way in.
    assert_eq!(
        one(
            &mut db,
            "MATCH (n:person) WHERE id(n) = 0 RETURN {who: n}.who AS v"
        ),
        one(&mut db, "MATCH (n:person) WHERE id(n) = 0 RETURN n AS v")
    );
}

/// A record is a value like any other, so the machinery that sorts and
/// groups values has to have an answer for it. ISO does not order two
/// records, but DISTINCT and ORDER BY still owe one, and the answer
/// has to be stable or a query returns different rows on two runs.
#[test]
fn records_sort_and_group_the_same_way_twice() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = graph(dir.path());
    let rows = run(
        "UNWIND [{a: 2}, {a: 1}, {a: 2}] AS r RETURN DISTINCT r AS v ORDER BY r",
        &mut db,
        &[],
    )
    .expect("distinct over records");
    assert_eq!(
        rows.rows,
        vec![
            vec![record(&[("a", Value::Int(1))])],
            vec![record(&[("a", Value::Int(2))])],
        ]
    );
}
