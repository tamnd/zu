//! The value query expression (ISO 20.6, feature GQ18).
//!
//! `VALUE { ... }` writes a whole query where one value belongs. What
//! is checked here is that the value is the one the query answered,
//! that the query inside is a query rather than a block (it may sort
//! and cut and aggregate), that a query answering nothing stands for a
//! null and one answering several rows is refused.
//!
//! The other half is what the query inside reads from the query around
//! it. One that reads nothing answers the same value for every row and
//! is worked out once, which is the decorrelation. One that reads a
//! name cannot be, so it runs per row and the statement comes back
//! with a warning saying so: the rows are right either way, and what
//! is wrong with the statement is what it costs.

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

/// The same fixture opened as a session, which is the way in that
/// hands back the warnings a statement raised beside its rows.
fn opened(name: &str) -> (tempfile::TempDir, zu::session::Session) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join(name);
    seeded(&path);
    let session = zu::session::Session::open(&path).expect("open");
    (dir, session)
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

/// A name from the query around it is a name this one may read, and
/// the value it stands for is the row's. Each of the five people finds
/// itself, so every row answers its own id.
#[test]
fn a_name_from_the_query_around_it_is_read_per_row() {
    let (_dir, mut session) = opened("value-correlated.zu1");
    let result = session
        .run(
            "MATCH (p:person) \
             RETURN VALUE { MATCH (q:person) WHERE q.id = p.id RETURN q.id } AS n \
             ORDER BY n",
            &[],
        )
        .expect("the statement runs");
    let ids: Vec<i64> = result
        .rows
        .iter()
        .map(|row| match row[0] {
            Value::Int(n) => n,
            ref other => panic!("expected an id, got {other:?}"),
        })
        .collect();
    assert_eq!(ids, [0, 1, 2, 3, 4]);
}

/// What it costs is the warning. A statement that could not have its
/// subquery worked out once comes back with the rows and with `01000`
/// beside them, naming what the query inside read, because that is the
/// thing to take out of it to get the cost back.
#[test]
fn a_query_that_does_not_decorrelate_is_warned_about() {
    let (_dir, mut session) = opened("value-warned.zu1");
    let result = session
        .run(
            "MATCH (p:person) \
             RETURN VALUE { MATCH (q:person) WHERE q.id = p.id RETURN q.id } AS n",
            &[],
        )
        .expect("the statement runs");
    let notice = result.notices.first().expect("a warning");
    assert_eq!(notice.status.code(), "01000", "{}", notice.detail);
    assert!(
        notice.detail.contains("once per row"),
        "{}, want what it costs",
        notice.detail
    );
    assert!(
        notice.detail.contains("reading p"),
        "{}, want the name it read",
        notice.detail
    );
}

/// The other side of the same test: a subquery reading nothing from
/// the rows around it carries no warning, because it was worked out
/// once and there is nothing to tell the caller.
#[test]
fn a_query_that_decorrelates_is_not_warned_about() {
    let (_dir, mut session) = opened("value-quiet.zu1");
    let result = session
        .run(
            "MATCH (p:person) \
             WHERE p.id < VALUE { MATCH (q:person) RETURN count(*) } \
             RETURN count(*) AS n",
            &[],
        )
        .expect("the statement runs");
    assert_eq!(result.rows[0][0], Value::Int(5));
    assert!(result.notices.is_empty(), "{:?}", result.notices);
}

/// The plan says which of the two a reader is looking at, so the cost
/// is read off the listing rather than off the clock: a subquery that
/// runs once says so, and one that runs per row says which names make
/// it do that.
#[test]
fn the_plan_says_how_often_the_query_inside_runs() {
    let (_dir, mut session) = opened("value-plan.zu1");
    let once = session
        .explain("MATCH (p:person) WHERE p.id < VALUE { MATCH (q:person) RETURN count(*) } RETURN p.id AS id")
        .expect("a plan");
    assert!(once.contains("VALUE {0} (once):"), "{once}");
    let each = session
        .explain(
            "MATCH (p:person) RETURN VALUE { MATCH (q:person) WHERE q.id = p.id RETURN q.id } AS n",
        )
        .expect("a plan");
    assert!(each.contains("VALUE {0} (per row, reading p):"), "{each}");
}

/// A name from two levels out is read the same way, by the level that
/// has it filling it in for the level that does not.
#[test]
fn a_name_two_levels_out_reaches_the_query_that_reads_it() {
    let (_dir, mut session) = opened("value-nested.zu1");
    let result = session
        .run(
            "MATCH (p:person) WHERE p.id = 3 \
             RETURN VALUE { MATCH (q:person) WHERE q.id = 0 \
             RETURN VALUE { MATCH (r:person) WHERE r.id = p.id RETURN r.id } } AS n",
            &[],
        )
        .expect("the statement runs");
    assert_eq!(result.rows[0][0], Value::Int(3));
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
