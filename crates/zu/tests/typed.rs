//! `expr IS TYPED type` end to end.
//!
//! The unit tests in `zu-query` check the membership rules. What is
//! only checkable here is that the syntax reaches them: that the type
//! grammar runs in predicate position as well as after `CAST`, that
//! the types with structure in them parse where a name would do, and
//! that a value type predicate is a boolean in a real row.

use zu::query::{Value, run};
use zu_zu1::file::Zu1File;
use zu_zu1::graph::bulk_load_as;

fn graph(dir: &std::path::Path) -> Zu1File {
    let mut zu = Zu1File::create(&dir.join("typed.zu1")).unwrap();
    bulk_load_as(&mut zu, "person", "knows", 2, &[(0, 1)]).unwrap();
    zu
}

/// The one row a `RETURN` of constants produces, as booleans.
fn row(db: &mut Zu1File, source: &str) -> Vec<Value> {
    let result = run(source, db, &[]).unwrap_or_else(|e| panic!("{source}: {e}"));
    assert_eq!(result.rows.len(), 1, "{source} returned {:?}", result.rows);
    result.rows[0].clone()
}

fn yes(db: &mut Zu1File, predicate: &str) {
    let source = format!("RETURN ({predicate}) AS v");
    assert_eq!(row(db, &source), vec![Value::Bool(true)], "{predicate}");
}

fn no(db: &mut Zu1File, predicate: &str) {
    let source = format!("RETURN ({predicate}) AS v");
    assert_eq!(row(db, &source), vec![Value::Bool(false)], "{predicate}");
}

#[test]
fn a_value_is_of_its_own_type_and_not_of_another() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = graph(dir.path());
    yes(&mut db, "1 IS TYPED INT");
    yes(&mut db, "'x' IS NOT TYPED INT");
    yes(&mut db, "'x' IS TYPED STRING");
    yes(&mut db, "true IS TYPED BOOL");
    yes(&mut db, "1.5 IS TYPED FLOAT64");
    yes(&mut db, "1 IS TYPED INT8 NOT NULL");
    no(&mut db, "1000 IS TYPED INT8");
    yes(&mut db, "'abc' IS TYPED STRING(2, 10)");
    no(&mut db, "'abc' IS TYPED CHAR(2)");
}

/// A cast reads a string as a number and the predicate does not, which
/// is the one difference that makes GA06 worth having next to GA05.
#[test]
fn the_predicate_asks_what_a_value_is_and_not_what_it_converts_to() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = graph(dir.path());
    assert_eq!(
        row(&mut db, "RETURN CAST('42' AS INT) AS v"),
        vec![Value::Int(42)]
    );
    no(&mut db, "'42' IS TYPED INT");
}

/// The byte string names, GV35 to GV38. zu has no byte string value,
/// so an integer is not one, which is the answer the corpus asks for
/// and the one that stays right when the value arrives.
#[test]
fn a_byte_string_type_takes_its_lengths_and_admits_no_integer() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = graph(dir.path());
    no(&mut db, "1 IS TYPED BYTES");
    no(&mut db, "1 IS TYPED BYTES(10)");
    no(&mut db, "1 IS TYPED BYTES(2, 10)");
    no(&mut db, "1 IS TYPED BINARY(4)");
}

#[test]
fn the_zoned_temporal_types_are_two_words_and_the_local_ones_say_so() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = graph(dir.path());
    no(&mut db, "1 IS TYPED ZONED DATETIME");
    no(&mut db, "1 IS TYPED ZONED TIME");
    no(&mut db, "1 IS TYPED LOCAL DATETIME");
    no(&mut db, "1 IS TYPED DATE");
}

#[test]
fn a_record_type_may_list_its_fields_and_a_field_may_be_a_record() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = graph(dir.path());
    no(&mut db, "1 IS TYPED ANY RECORD");
    no(&mut db, "1 IS TYPED RECORD { a :: INT }");
    no(&mut db, "1 IS TYPED RECORD { a :: RECORD { b :: INT } }");
    no(&mut db, "1 IS TYPED PATH");
    no(&mut db, "1 IS TYPED ANY GRAPH");
    no(&mut db, "1 IS TYPED BINDING TABLE");
}

#[test]
fn a_node_is_of_the_node_type_and_a_path_is_of_the_path_type() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = graph(dir.path());
    let source = "MATCH (p:person) RETURN count(p) AS n";
    assert_eq!(row(&mut db, source), vec![Value::Int(2)]);
    let source = "MATCH (p:person) WHERE p IS TYPED ANY VALUE RETURN count(p) AS n";
    assert_eq!(row(&mut db, source), vec![Value::Int(2)]);
    // A node is not a property value, which is what keeps GV68 from
    // being a second spelling of the open union.
    let source = "MATCH (p:person) WHERE p IS TYPED ANY PROPERTY VALUE RETURN count(p) AS n";
    assert_eq!(row(&mut db, source), vec![Value::Int(0)]);
}

#[test]
fn the_dynamic_unions_admit_what_their_members_admit() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = graph(dir.path());
    yes(&mut db, "1 IS TYPED ANY VALUE");
    yes(&mut db, "1 IS TYPED ANY PROPERTY VALUE");
    yes(&mut db, "1 IS TYPED INT | STRING");
    yes(&mut db, "'x' IS TYPED INT | STRING");
    no(&mut db, "true IS TYPED INT | STRING");
}

/// The immaterial types and explicit nullability, GV71, GV72 and GV90.
/// The predicate answers for a null rather than returning one, which is
/// the whole reason a query can ask.
#[test]
fn the_null_value_belongs_to_a_nullable_type_and_to_nothing_else() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = graph(dir.path());
    yes(&mut db, "NULL IS TYPED NULL");
    no(&mut db, "1 IS TYPED NULL");
    no(&mut db, "1 IS TYPED NOTHING");
    no(&mut db, "NULL IS TYPED NOTHING");
    yes(&mut db, "NULL IS TYPED INT");
    no(&mut db, "NULL IS TYPED INT NOT NULL");
}

#[test]
fn a_predicate_in_a_filter_agrees_between_the_two_executors() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = graph(dir.path());
    let source = "MATCH (p:person) WHERE 1 IS TYPED INT RETURN count(p) AS n";
    let with_exec2 = run(source, &mut db, &[]).unwrap();
    // SAFETY: single-threaded test, and the variable is read back by
    // this process only.
    unsafe { std::env::set_var("ZU_EXEC2", "0") };
    let without = run(source, &mut db, &[]).unwrap();
    unsafe { std::env::remove_var("ZU_EXEC2") };
    assert_eq!(with_exec2.rows, without.rows);
    assert_eq!(with_exec2.rows[0][0], Value::Int(2));
}

#[test]
fn an_unknown_type_name_in_a_predicate_is_a_syntax_error() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = graph(dir.path());
    let err = run("RETURN (1 IS TYPED NOSUCHTYPE) AS v", &mut db, &[]).expect_err("should fail");
    assert_eq!(err.gqlstatus().unwrap().code(), "42001");
    let err = run("RETURN (1 IS TYPED RECORD { a : INT }) AS v", &mut db, &[])
        .expect_err("a single colon is not a field separator");
    assert_eq!(err.gqlstatus().unwrap().code(), "42001");
}
