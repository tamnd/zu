//! Reading a real result as Rust types, the way `dx/04` §4 writes it.
//!
//! The unit tests in `zu_query::row` build results by hand, which
//! checks the conversions but not the thing a caller actually does.
//! What is checked here is the whole path: a statement runs on a
//! connection, the columns it projected are the names the caller reads
//! by, the strings come back borrowed from the result rather than
//! copied out of it, and a parameter written with `params!` binds
//! without the caller ever spelling a `Value`.

use zu::query::Value;
use zu::zu1::file::Zu1File;
use zu::zu1::graph::bulk_load_as;
use zu::{Database, params};

const NODES: u32 = 200;

fn seeded(path: &std::path::Path) {
    let mut db = Zu1File::create(path).expect("create");
    let mut edges: Vec<(u32, u32)> = (0..NODES)
        .flat_map(|i| [(i, (i + 1) % NODES), (i, (i + 7) % NODES)])
        .collect();
    edges.sort_unstable();
    bulk_load_as(&mut db, "person", "knows", NODES.into(), &edges).expect("load");
}

#[test]
fn a_result_reads_as_tuples_and_the_strings_are_not_copied() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("rows.zu1");
    seeded(&path);
    let db = Database::open(&path).expect("open");
    let mut conn = db.connect().expect("connect");

    let rows = conn
        .query(
            "MATCH (p:person)-[:knows]->(f) RETURN p.id AS id, count(f) AS n ORDER BY id LIMIT 5",
        )
        .expect("query");
    let mut seen = Vec::new();
    for row in rows.iter() {
        let (id, n): (i64, i64) = row.get().expect("two integer columns");
        seen.push((id, n));
    }
    assert_eq!(seen, vec![(0, 2), (1, 2), (2, 2), (3, 2), (4, 2)]);

    // A borrowed string outlives the row it came from and not the
    // result, which is the whole point of the lifetime: this compiles
    // because `names` borrows `rows`, and would not if `Row` owned
    // anything.
    let rows = conn
        .query("UNWIND ['ada', 'bob'] AS name RETURN name")
        .expect("query");
    let names: Vec<&str> = rows
        .iter()
        .map(|row| row.get_by_name("name").expect("a string column"))
        .collect();
    assert_eq!(names, vec!["ada", "bob"]);
}

#[test]
fn parameters_written_with_the_macro_bind_by_the_names_the_statement_uses() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("params.zu1");
    seeded(&path);
    let db = Database::open(&path).expect("open");
    let mut conn = db.connect().expect("connect");

    // A list parameter and a scalar in one call, neither of them
    // spelled as a `Value` by the caller.
    let rows = conn
        .query_with(
            "MATCH (p:person) WHERE p.id IN $ids AND p.id > $floor RETURN count(p) AS n",
            &params! { "ids" => vec![1, 2, 3, 4], "floor" => 2 },
        )
        .expect("query");
    let n: i64 = rows.row(0).expect("a row").get_at(0).expect("a count");
    assert_eq!(n, 2);

    // The same bindings through a prepared statement, which takes the
    // same slice, so the macro serves both paths.
    let (stmt, wanted) = conn
        .prepare("MATCH (p:person {id: $id}) RETURN p.id AS id")
        .expect("prepare");
    assert_eq!(wanted, vec!["id".to_string()]);
    let rows = conn
        .execute_prepared(stmt, &params! { "id" => 9 })
        .expect("execute");
    let (id,): (i64,) = rows.row(0).expect("a row").get().expect("one column");
    assert_eq!(id, 9);
}

#[test]
fn asking_a_column_for_the_wrong_type_names_the_column_and_the_two_types() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("wrong.zu1");
    seeded(&path);
    let db = Database::open(&path).expect("open");
    let mut conn = db.connect().expect("connect");

    let rows = conn
        .query("MATCH (p:person) RETURN p.id AS id ORDER BY id LIMIT 1")
        .expect("query");
    let row = rows.row(0).expect("a row");
    let err = row.get_at::<&str>(0).expect_err("an int is not a string");
    assert_eq!(err.gqlstatus(), Some(zu::gqlstatus::codes::C22G03));
    let message = err.to_string();
    assert!(message.contains("column 'id'"), "{message}");
    assert!(message.contains("STRING"), "{message}");
    assert!(message.contains("INT64"), "{message}");

    // And the untyped values are still there for a caller that wants to
    // match on them itself, which is what the typed layer is built on
    // rather than a replacement for.
    assert_eq!(row.value(0).expect("a value"), &Value::Int(0));
    assert_eq!(row.columns(), ["id".to_string()]);
}
