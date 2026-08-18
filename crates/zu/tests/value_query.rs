//! The value query expression (ISO 20.6, feature GQ18).
//!
//! `VALUE { ... }` writes a whole query where one value belongs. What
//! is checked here is that the value is the one the query answered,
//! that the query inside is a query rather than a block (it may sort
//! and cut and aggregate), that a query answering nothing stands for a
//! null and one answering several rows is refused, and that the two
//! queries share nothing: a name from the query around it is not a
//! name this one can read.

use zu::Database;
use zu::zu1::file::Zu1File;
use zu::zu1::graph::bulk_load_as;
use zu_query::exec::Value;

const NODES: u32 = 5;

/// Five people in a ring, so `count(*)` over them is five and their
/// ids are 0 to 4.
fn seeded(path: &std::path::Path) {
    let mut db = Zu1File::create(path).expect("create");
    let edges: Vec<(u32, u32)> = (0..NODES).map(|i| (i, (i + 1) % NODES)).collect();
    bulk_load_as(&mut db, "person", "knows", NODES.into(), &edges).expect("load");
}

struct Fixture {
    _dir: tempfile::TempDir,
    conn: zu::Connection,
}

impl Fixture {
    fn open(name: &str) -> Fixture {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(name);
        seeded(&path);
        let db = Database::open(&path).expect("open");
        let conn = db.connect().expect("connect");
        Fixture { _dir: dir, conn }
    }

    fn one(&mut self, source: &str) -> i64 {
        let rows = self.conn.query(source).expect("query");
        let row = rows.iter().next().expect("a row");
        row.get_by_name::<i64>("n").expect("n")
    }

    fn error(&mut self, source: &str) -> String {
        self.conn
            .query(source)
            .expect_err("this one does not run")
            .to_string()
    }
}

/// The corpus case: a whole query standing for one value, in a
/// statement that has nothing else in it.
#[test]
fn a_value_query_stands_for_the_value_it_answered() {
    let mut fx = Fixture::open("value-count.zu1");
    assert_eq!(
        fx.one("RETURN VALUE { MATCH (p:person) RETURN count(*) } AS n"),
        5
    );
}

/// The value is the same for every row it is read on, which is what
/// lets the engine work it out once. Five ids, all of them under the
/// five the subquery counted.
#[test]
fn every_row_reads_the_same_value() {
    let mut fx = Fixture::open("value-rows.zu1");
    assert_eq!(
        fx.one(
            "MATCH (p:person) \
             WHERE p.id < VALUE { MATCH (q:person) RETURN count(*) } \
             RETURN count(*) AS n"
        ),
        5
    );
}

/// What is inside is a query and not a block, so it may sort its rows
/// and cut them down to the one it stands for.
#[test]
fn the_query_inside_is_a_whole_query() {
    let mut fx = Fixture::open("value-limit.zu1");
    assert_eq!(
        fx.one("RETURN VALUE { MATCH (p:person) RETURN p.id ORDER BY p.id DESC LIMIT 1 } AS n"),
        4
    );
}

/// A query that answered no row stands for a null, so the predicate
/// reading it has a null to work with rather than an error.
#[test]
fn a_query_that_answers_nothing_stands_for_a_null() {
    let mut fx = Fixture::open("value-null.zu1");
    assert_eq!(
        fx.one(
            "MATCH (p:person) \
             WHERE VALUE { MATCH (q:person) WHERE q.id > 100 RETURN q.id } IS NULL \
             RETURN count(*) AS n"
        ),
        5
    );
}

/// One value is what was written, so a query answering several rows is
/// refused with the two ways of writing what was meant.
#[test]
fn several_rows_are_refused() {
    let mut fx = Fixture::open("value-many.zu1");
    let err = fx.error("RETURN VALUE { MATCH (p:person) RETURN p.id } AS n");
    assert!(err.contains("answered 5 rows"), "{err}, want how many");
    assert!(err.contains("LIMIT 1"), "{err}, want the way out");
}

/// One value is one column, and this is known while the statement is
/// being read rather than after it has run.
#[test]
fn several_columns_are_refused() {
    let mut fx = Fixture::open("value-columns.zu1");
    let err = fx.error("RETURN VALUE { MATCH (p:person) RETURN p.id AS a, p.id AS b } AS n");
    assert!(err.contains("has to return one column"), "{err}");
}

/// The two queries share nothing but their parameters, and a name from
/// the query around this one is refused by saying what is wrong with
/// it: the name exists, and reading it is what this engine cannot do.
#[test]
fn a_name_from_the_query_around_it_is_refused() {
    let mut fx = Fixture::open("value-correlated.zu1");
    let err = fx.error(
        "MATCH (p:person) \
         RETURN VALUE { MATCH (q:person) WHERE q.id = p.id RETURN q.id } AS n",
    );
    assert!(
        err.contains("cannot read 'p' from the query around it"),
        "{err}, want the name and why"
    );
}

/// It stands where a value belongs and is read there, so it may not
/// change the graph on the way.
#[test]
fn a_value_query_may_not_write() {
    let mut fx = Fixture::open("value-write.zu1");
    let err = fx.error("RETURN VALUE { INSERT (x:person) RETURN 1 } AS n");
    assert!(err.contains("may not write to the graph"), "{err}");
}

/// A parameter is the one thing the two queries do share, and it is
/// the same position in both.
#[test]
fn a_parameter_reaches_the_query_inside() {
    let mut fx = Fixture::open("value-param.zu1");
    let rows = fx
        .conn
        .query_with(
            "RETURN VALUE { MATCH (p:person) WHERE p.id < $cut RETURN count(*) } AS n",
            &[("cut", Value::Int(3))],
        )
        .expect("query");
    let row = rows.iter().next().expect("a row");
    assert_eq!(row.get_by_name::<i64>("n").expect("n"), 3);
}
