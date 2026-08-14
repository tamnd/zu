//! Where the null value sorts, ISO subclause 16.17 and feature GA03.
//!
//! Nulls reach a sort key two ways, from a property a node does not
//! hold and from a pattern that did not match, and both are asked the
//! same questions here: the implicit ordering, `NULLS FIRST`, `NULLS
//! LAST`, and what a descending key does to all three. Both shapes run
//! on the row engine today, the first because the vector scan declines
//! a nullable column and the second because it declines `OPTIONAL`, so
//! the pipeline sort's own null handling is covered where it lives, in
//! the sink's tests.

use zu::convert::sqlite_to_zu1;
use zu::query::run;
use zu_sqlite::{ColumnType, SqliteStore, Value as SqlValue};
use zu_zu1::file::Zu1File;

/// Five people, three of whom have an age. The two without are the
/// rows every case here is about, and the names are distinct so a tie
/// never decides anything.
const AGES: [Option<i64>; 5] = [Some(30), None, Some(10), None, Some(20)];
const NAMES: [&str; 5] = ["ada", "bob", "cat", "dan", "eve"];

fn seed(path: &std::path::Path) {
    let mut sq = SqliteStore::open(path).unwrap();
    sq.create_node_table(
        "person",
        &[("age", ColumnType::Integer), ("name", ColumnType::Text)],
    )
    .unwrap();
    sq.create_rel_table("knows", "person", "person", &[])
        .unwrap();
    sq.begin().unwrap();
    for row in 0..5i64 {
        let age = match AGES[row as usize] {
            Some(n) => SqlValue::Int(n),
            None => SqlValue::Null,
        };
        sq.insert_node_at(
            "person",
            row,
            &[age, SqlValue::Text(NAMES[row as usize].to_owned())],
        )
        .unwrap();
    }
    // ada knows bob, cat knows eve, eve knows ada. Two people know
    // nobody, which is where the second kind of null comes from.
    for (src, dst) in [(0, 1), (2, 4), (4, 0)] {
        sq.insert_rel("knows", src, dst, &[]).unwrap();
    }
    sq.commit().unwrap();
}

/// The names a query returns, in the order it returns them.
fn names(query: &str, db: &mut Zu1File) -> Vec<String> {
    run(query, db, &[])
        .unwrap()
        .rows
        .into_iter()
        .map(|row| match row.into_iter().next_back() {
            Some(zu_query::exec::Value::Str(s)) => s,
            other => panic!("the last column is not a name: {other:?}"),
        })
        .collect()
}

fn open(dir: &tempfile::TempDir) -> Zu1File {
    let db = dir.path().join("a.db");
    seed(&db);
    let out = dir.path().join("a.zu1");
    sqlite_to_zu1(&db, &out).unwrap();
    Zu1File::open(&out).unwrap()
}

#[test]
fn a_key_that_says_nothing_puts_its_nulls_last() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = open(&dir);
    assert_eq!(
        names(
            "MATCH (p:person) RETURN p.age AS age, p.name AS name ORDER BY age, name",
            &mut db
        ),
        ["cat", "eve", "ada", "bob", "dan"],
        "the implicit ordering is last, which is impdef IS001"
    );
    // Descending reverses the values and leaves the nulls where they
    // were. The implicit ordering is one answer, not one per direction.
    assert_eq!(
        names(
            "MATCH (p:person) RETURN p.age AS age, p.name AS name ORDER BY age DESC, name",
            &mut db
        ),
        ["ada", "eve", "cat", "bob", "dan"]
    );
}

#[test]
fn nulls_first_and_nulls_last_say_it_outright() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = open(&dir);
    assert_eq!(
        names(
            "MATCH (p:person) RETURN p.age AS age, p.name AS name ORDER BY age NULLS FIRST, name",
            &mut db
        ),
        ["bob", "dan", "cat", "eve", "ada"]
    );
    assert_eq!(
        names(
            "MATCH (p:person) RETURN p.age AS age, p.name AS name ORDER BY age NULLS LAST, name",
            &mut db
        ),
        ["cat", "eve", "ada", "bob", "dan"]
    );
    // The direction and the null ordering are independent halves of
    // one sort specification, so this is the values downwards with the
    // nulls still at the head.
    assert_eq!(
        names(
            "MATCH (p:person) RETURN p.age AS age, p.name AS name \
             ORDER BY age DESC NULLS FIRST, name",
            &mut db
        ),
        ["bob", "dan", "ada", "eve", "cat"]
    );
}

#[test]
fn a_limit_returns_the_full_orders_prefix() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = open(&dir);
    // A bounded sort judges a row on its keys alone and drops the
    // losers before building them, so a null reaching the answer at
    // all is the buffer agreeing with the full sort about where it
    // sits.
    for (order, want) in [
        ("age NULLS FIRST, name", ["bob", "dan"]),
        ("age, name", ["cat", "eve"]),
        ("age DESC NULLS FIRST, name", ["bob", "dan"]),
    ] {
        let query = format!(
            "MATCH (p:person) RETURN p.age AS age, p.name AS name ORDER BY {order} LIMIT 2"
        );
        assert_eq!(names(&query, &mut db), want, "{order}");
    }
}

#[test]
fn a_null_a_pattern_did_not_match_sorts_the_same_way() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = open(&dir);
    // A null the pattern made rather than one a column recorded. The
    // sort cannot tell them apart and neither can the key, which is
    // the point.
    let q = |order: &str| {
        format!(
            "MATCH (p:person) OPTIONAL MATCH (p)-[:knows]->(f:person) \
             RETURN f.name AS tag, p.name AS name ORDER BY {order}"
        )
    };
    assert_eq!(
        names(&q("tag, name"), &mut db),
        ["eve", "ada", "cat", "bob", "dan"]
    );
    assert_eq!(
        names(&q("tag NULLS FIRST, name"), &mut db),
        ["bob", "dan", "eve", "ada", "cat"]
    );
    assert_eq!(
        names(&q("tag DESC NULLS FIRST, name"), &mut db),
        ["bob", "dan", "cat", "ada", "eve"]
    );
    assert_eq!(
        names(&q("tag DESC NULLS FIRST, name LIMIT 3"), &mut db),
        ["bob", "dan", "cat"]
    );
}

#[test]
fn a_key_that_names_neither_first_nor_last_is_a_syntax_error() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = open(&dir);
    let err = run(
        "MATCH (p:person) RETURN p.name AS name ORDER BY name NULLS SOMEWHERE",
        &mut db,
        &[],
    )
    .unwrap_err();
    assert!(format!("{err}").contains("LAST"), "unexpected error: {err}");
}
