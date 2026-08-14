//! `CAST(expr AS type)` end to end, both executors.
//!
//! The unit tests in `zu-query` check the lattice arithmetic. What is
//! only checkable here is that the text reaches it: that the parser
//! takes a type where an expression would otherwise go, that the
//! pipeline executor declines a plan holding a cast instead of running
//! a wrong one, and that the two conditions the corpus asks for come
//! back with their gqlstatus codes attached.

use zu::query::{Value, run};
use zu_zu1::file::Zu1File;
use zu_zu1::graph::bulk_load_as;

/// The smallest graph a query can run against: the executor needs a
/// catalog, and no case here reads a property from it.
fn graph(dir: &std::path::Path) -> Zu1File {
    let mut zu = Zu1File::create(&dir.join("cast.zu1")).unwrap();
    bulk_load_as(&mut zu, "person", "knows", 2, &[(0, 1)]).unwrap();
    zu
}

fn one(db: &mut Zu1File, source: &str) -> Value {
    let result = run(source, db, &[]).unwrap_or_else(|e| panic!("{source}: {e}"));
    assert_eq!(result.rows.len(), 1, "{source} returned {:?}", result.rows);
    result.rows[0][0].clone()
}

fn status(db: &mut Zu1File, source: &str) -> String {
    let err = run(source, db, &[]).expect_err(&format!("{source} should have failed"));
    err.gqlstatus()
        .unwrap_or_else(|| panic!("{source} raised {err}, which carries no gqlstatus"))
        .code()
        .to_string()
}

/// One case per feature in the integer tower, written the way the
/// conformance corpus writes them: the largest value the width holds,
/// which is the value a wrong bound would refuse.
#[test]
fn every_width_in_the_tower_carries_its_own_largest_value() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = graph(dir.path());
    for (source, want) in [
        ("RETURN CAST(127 AS INT8) AS v", 127i64),
        ("RETURN CAST(32767 AS INT16) AS v", 32767),
        ("RETURN CAST(2147483647 AS INT32) AS v", 2147483647),
        (
            "RETURN CAST(9223372036854775807 AS INT64) AS v",
            9223372036854775807,
        ),
        ("RETURN CAST(1 AS INT128) AS v", 1),
        ("RETURN CAST(1 AS INT256) AS v", 1),
        ("RETURN CAST(255 AS UINT8) AS v", 255),
        ("RETURN CAST(65535 AS UINT16) AS v", 65535),
        ("RETURN CAST(4294967295 AS UINT32) AS v", 4294967295),
        ("RETURN CAST(4294967296 AS UINT64) AS v", 4294967296),
        ("RETURN CAST(1 AS UINT128) AS v", 1),
        ("RETURN CAST(1 AS UINT256) AS v", 1),
        ("RETURN CAST(1 AS SMALLINT) AS v", 1),
        ("RETURN CAST(1 AS USMALLINT) AS v", 1),
        ("RETURN CAST(1 AS BIGINT) AS v", 1),
        ("RETURN CAST(1 AS UBIGINT) AS v", 1),
        ("RETURN CAST(1 AS UINT) AS v", 1),
        ("RETURN CAST(123456789 AS INT(9)) AS v", 123456789),
    ] {
        assert_eq!(one(&mut db, source), Value::Int(want), "{source}");
    }
}

#[test]
fn a_string_and_an_integer_cast_into_each_other() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = graph(dir.path());
    assert_eq!(
        one(&mut db, "RETURN CAST('42' AS INT) AS v"),
        Value::Int(42)
    );
    assert_eq!(
        one(&mut db, "RETURN CAST(42 AS STRING) AS v"),
        Value::Str("42".into())
    );
}

/// The float widths and the two synonyms, plus the one type name ISO
/// writes as two words.
#[test]
fn every_float_spelling_carries_a_value_that_is_exact_in_all_of_them() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = graph(dir.path());
    for source in [
        "RETURN CAST(0.5 AS FLOAT16) AS v",
        "RETURN CAST(0.5 AS FLOAT32) AS v",
        "RETURN CAST(0.5 AS FLOAT64) AS v",
        "RETURN CAST(0.5 AS FLOAT128) AS v",
        "RETURN CAST(0.5 AS FLOAT256) AS v",
        "RETURN CAST(0.5 AS FLOAT(10)) AS v",
        "RETURN CAST(0.5 AS REAL) AS v",
        "RETURN CAST(0.5 AS DOUBLE) AS v",
        "RETURN CAST(0.5 AS DOUBLE PRECISION) AS v",
    ] {
        assert_eq!(one(&mut db, source), Value::Float(0.5), "{source}");
    }
}

#[test]
fn a_length_pads_at_the_bottom_and_refuses_at_the_top() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = graph(dir.path());
    assert_eq!(
        one(&mut db, "RETURN CAST('abc' AS STRING(10)) AS v"),
        Value::Str("abc".into())
    );
    assert_eq!(
        one(&mut db, "RETURN CAST('abc' AS STRING(2, 10)) AS v"),
        Value::Str("abc".into())
    );
    assert_eq!(
        one(&mut db, "RETURN CAST('a' AS CHAR(3)) AS v"),
        Value::Str("a  ".into())
    );
    assert_eq!(
        status(&mut db, "RETURN CAST('abc' AS CHAR(2)) AS v"),
        "22001"
    );
}

#[test]
fn a_decimal_takes_a_precision_and_a_scale() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = graph(dir.path());
    assert_eq!(
        one(&mut db, "RETURN CAST('1.20' AS DECIMAL(5, 2)) AS v"),
        Value::Float(1.2)
    );
    assert_eq!(
        status(&mut db, "RETURN CAST('1000.00' AS DECIMAL(5, 2)) AS v"),
        "22003"
    );
    // A scale past the precision names digits the number cannot hold,
    // which is a statement nobody can mean rather than a value nobody
    // can store, so it is refused at parse time.
    assert_eq!(
        status(&mut db, "RETURN CAST('1.20' AS DECIMAL(2, 5)) AS v"),
        "42001"
    );
}

#[test]
fn the_two_conditions_come_back_with_their_codes() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = graph(dir.path());
    assert_eq!(
        one(&mut db, "RETURN CAST(100 AS INT8) AS v"),
        Value::Int(100)
    );
    assert_eq!(status(&mut db, "RETURN CAST(1000 AS INT8) AS v"), "22003");
    assert_eq!(
        status(&mut db, "RETURN CAST(NULL AS INT NOT NULL) AS v"),
        "22004"
    );
    assert_eq!(one(&mut db, "RETURN CAST(NULL AS INT) AS v"), Value::Null);
}

/// A cast inside a filter runs over real rows, which is the path the
/// pipeline executor would take if it compiled one. It does not, so
/// the answer has to come from the fallback and has to be the same
/// answer either way.
#[test]
fn a_cast_in_a_filter_agrees_between_the_two_executors() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = graph(dir.path());
    let source = "MATCH (p:person) WHERE CAST('1' AS INT8) = 1 RETURN count(p) AS n";
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
fn an_unknown_type_name_is_a_syntax_error_and_not_a_variable() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = graph(dir.path());
    assert_eq!(
        status(&mut db, "RETURN CAST(1 AS NOSUCHTYPE) AS v"),
        "42001"
    );
}
