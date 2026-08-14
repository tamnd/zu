//! Universal comparison, feature GA04, as one total order.
//!
//! ISO says every two values are comparable and does not say which of
//! two types comes first, so the precedence is zu's answer to IV010 and
//! these cases are what pins it down. The point of the file is not the
//! precedence itself, which another engine may choose differently, but
//! that there is exactly one of it: the answer `x < y` gives and the
//! order `ORDER BY x` produces are read from the same table, and a
//! query can rely on the two agreeing.

use zu::query::{Value, run};
use zu_zu1::file::Zu1File;

fn db(dir: &std::path::Path) -> Zu1File {
    Zu1File::create(&dir.join("comparison.zu1")).unwrap()
}

fn one(db: &mut Zu1File, source: &str) -> Value {
    let result = run(source, db, &[]).unwrap_or_else(|e| panic!("{source}: {e}"));
    assert_eq!(result.rows.len(), 1, "{source} returned {:?}", result.rows);
    result.rows[0][0].clone()
}

/// Whether the expression came back true.
fn yes(db: &mut Zu1File, source: &str) -> bool {
    match one(db, &format!("RETURN ({source}) AS v")) {
        Value::Bool(b) => b,
        other => panic!("{source} is {other:?}, not a truth value"),
    }
}

/// One value written per type, in the order zu puts them in. A
/// comparison between any two of these is a comparison between two
/// types, which is the whole subject here.
const TOWER: [&str; 6] = ["FALSE", "1", "'a'", "DATE '2024-01-15'", "[1]", "{ a: 1 }"];

#[test]
fn a_value_of_one_type_compares_with_a_value_of_another() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = db(dir.path());
    // Every earlier member of the tower is less than every later one,
    // and the answer is a truth value rather than a null: a query that
    // asks which of two values comes first gets told.
    for (i, low) in TOWER.iter().enumerate() {
        for high in &TOWER[i + 1..] {
            assert!(yes(&mut db, &format!("{low} < {high}")), "{low} < {high}");
            assert!(yes(&mut db, &format!("{high} > {low}")), "{high} > {low}");
            assert!(
                !yes(&mut db, &format!("{low} >= {high}")),
                "{low} >= {high}"
            );
        }
    }
}

#[test]
fn a_comparison_and_a_sort_read_the_same_order() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = db(dir.path());
    // The sort puts the tower back in the order the operators said it
    // was in. Two orders is the failure this case exists to catch: an
    // engine can answer `1 < 'a'` one way in a predicate and another in
    // a sort key without either half looking wrong on its own.
    let sorted = run(
        &format!(
            "UNWIND [{}] AS x RETURN x AS v ORDER BY x",
            TOWER.iter().rev().cloned().collect::<Vec<_>>().join(", ")
        ),
        &mut db,
        &[],
    )
    .unwrap();
    let want = run(
        &format!("UNWIND [{}] AS x RETURN x AS v", TOWER.join(", ")),
        &mut db,
        &[],
    )
    .unwrap();
    assert_eq!(sorted.rows, want.rows);
}

#[test]
fn two_values_of_one_type_keep_the_types_own_order() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = db(dir.path());
    // The type precedence decides nothing here, so these are the
    // ordinary answers and the cross type rule has not disturbed them.
    for source in [
        "1 < 2",
        "1 < 1.5",
        "-1.5 < 1",
        "FALSE < TRUE",
        "'a' < 'b'",
        "DATE '2024-01-15' < DATE '2024-02-01'",
        "[1, 2] < [1, 3]",
        "[1] < [1, 0]",
        "{ a: 1 } < { a: 2 }",
        "{ a: 1 } < { b: 0 }",
    ] {
        assert!(yes(&mut db, source), "{source}");
    }
}

/// A month and thirty days are two duration kinds and no number of
/// days is a month, so ISO leaves their order to the implementation
/// (IV002) and zu answers it with the kind rather than with a null.
#[test]
fn two_durations_of_different_kinds_still_have_an_order() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = db(dir.path());
    assert!(yes(&mut db, "DURATION 'P1M' < DURATION 'P30D'"));
    assert!(!yes(&mut db, "DURATION 'P1M' = DURATION 'P30D'"));
    // Within one kind the count decides, which is the order the
    // durations had before the kinds were put in a sequence.
    assert!(yes(&mut db, "DURATION 'P1D' < DURATION 'P2D'"));
    assert!(yes(&mut db, "DURATION 'P1M' < DURATION 'P2M'"));
}

#[test]
fn exactly_one_of_less_equal_and_greater_holds() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = db(dir.path());
    // Equality and ordering are two readings of one order, so a pair
    // that compares equal is neither less nor greater and a pair that
    // does not is one of them. This is what an engine loses when its
    // `=` and its `<` are written twice.
    for (a, b) in [
        ("1", "1.0"),
        ("1", "2"),
        ("1", "'a'"),
        ("'a'", "FALSE"),
        ("[1]", "{ a: 1 }"),
        ("DATE '2024-01-15'", "1"),
        ("[1, 2]", "[1, 2]"),
    ] {
        let held = ["<", "=", ">"]
            .iter()
            .filter(|op| yes(&mut db, &format!("{a} {op} {b}")))
            .count();
        assert_eq!(held, 1, "{a} against {b}");
    }
}

#[test]
fn a_comparison_against_null_is_still_unknown() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = db(dir.path());
    // The total order has a place for the null and a query never sees
    // it. Every operator answers unknown, which is the null, and the
    // three valued logic that follows is unchanged by any of this.
    for source in ["NULL < 1", "1 < NULL", "NULL >= 'a'", "NULL = NULL"] {
        assert_eq!(
            one(&mut db, &format!("RETURN ({source}) AS v")),
            Value::Null,
            "{source}"
        );
    }
    // A list holding a null is a value like any other and the order
    // walks it positionally, so this is an answer rather than unknown.
    assert!(yes(&mut db, "[NULL] < [NULL, 1]"));
}

/// GA01. A NaN is not a number any comparison can place, and the total
/// order still owes an answer, so it sits after every number.
#[test]
fn a_nan_sorts_after_every_number() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = db(dir.path());
    // Division by zero raises 22012 rather than answering, so the way
    // to a NaN is through two infinities of the same sign.
    let nan = "(1e308 * 10 - 1e308 * 10)";
    assert!(yes(&mut db, &format!("{nan} > 1e308")));
    assert!(yes(&mut db, &format!("{nan} > 0")));
    assert!(!yes(&mut db, &format!("{nan} < {nan}")));
    // Two NaNs sort equal under the order and are still not equal
    // values, because IEEE equality is a rule about the value and the
    // order is a rule about the sequence.
    assert!(!yes(&mut db, &format!("{nan} = {nan}")));
    assert!(yes(&mut db, &format!("{nan} >= {nan}")));
}
