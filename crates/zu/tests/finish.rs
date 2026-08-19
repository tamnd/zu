//! `FINISH`, the result statement that says there is no result (ISO
//! subclause 14.10).
//!
//! It is the other way a query may end. A `RETURN` says what the
//! answer is, and this says that the answer is nothing, which is not
//! the same as a query that failed and not the same as a query that
//! answered zero rows: the clauses in front of it ran, and a write in
//! one of them wrote. What is checked here is that the statement runs,
//! that it answers no columns and no rows, and that the words which
//! read a result are refused behind it.

use zu::Database;
use zu::zu1::file::Zu1File;
use zu::zu1::graph::bulk_load_as;

const NODES: u32 = 4;

struct Fixture {
    _dir: tempfile::TempDir,
    conn: zu::Connection,
}

impl Fixture {
    fn open(name: &str) -> Fixture {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(name);
        let mut db = Zu1File::create(&path).expect("create");
        let edges: Vec<(u32, u32)> = (0..NODES).map(|i| (i, (i + 1) % NODES)).collect();
        bulk_load_as(&mut db, "person", "knows", NODES.into(), &edges).expect("load");
        drop(db);
        let db = Database::open(&path).expect("open");
        let conn = db.connect().expect("connect");
        Fixture { _dir: dir, conn }
    }
}

/// A query that ends with FINISH answers a table with no columns and
/// no rows, and it answers rather than failing.
#[test]
fn a_finished_query_answers_nothing() {
    let mut fx = Fixture::open("finish.zu1");
    let rows = fx
        .conn
        .query("MATCH (p:person) FINISH")
        .expect("a query that returns nothing still runs");
    assert_eq!(rows.rows.len(), 0, "no rows");
    assert!(rows.columns.is_empty(), "no columns");
}

/// The rows the clauses in front of it answered are gone whatever they
/// were, so a match that found nothing and a match that found
/// everything end the same way.
#[test]
fn what_it_found_makes_no_difference() {
    let mut fx = Fixture::open("finish-empty.zu1");
    let rows = fx
        .conn
        .query("MATCH (p:person) FILTER p.id > 1000 FINISH")
        .expect("query");
    assert_eq!(rows.rows.len(), 0);
    assert_eq!(rows.rows.len(), 0, "no rows");
    assert!(rows.columns.is_empty(), "no columns");
}

/// Nothing may read from it. NEXT hands one statement's result to the
/// next and a conjunction joins two of them, and the statement in
/// front of either has to have a result to hand over.
#[test]
fn nothing_reads_what_it_did_not_return() {
    let mut fx = Fixture::open("finish-next.zu1");
    let err = fx
        .conn
        .query("MATCH (p:person) FINISH NEXT RETURN 1 AS one")
        .expect_err("NEXT reads a result and there is none");
    assert!(
        err.to_string().contains("FINISH"),
        "{err}, want the word that ended the statement"
    );
    let err = fx
        .conn
        .query("MATCH (p:person) FINISH UNION MATCH (q:person) RETURN q.id AS id")
        .expect_err("a conjunction joins two result tables");
    assert!(
        err.to_string().contains("RETURN"),
        "{err}, want what the left operand was missing"
    );
}

/// It is a whole statement rather than a word that swallows what
/// follows it, so text after it is refused the way text after a
/// RETURN is.
#[test]
fn nothing_may_follow_it() {
    let mut fx = Fixture::open("finish-tail.zu1");
    let err = fx
        .conn
        .query("MATCH (p:person) FINISH RETURN 1 AS one")
        .expect_err("the statement ended");
    assert!(
        err.to_string().contains("nothing may follow FINISH"),
        "{err}, want the reason"
    );
}

/// A write in front of it wrote. That is the whole reason the standard
/// has the statement: the query says it wants nothing back, not that it
/// wants nothing done.
#[test]
fn a_write_in_front_of_it_wrote() {
    let mut fx = Fixture::open("finish-write.zu1");
    let rows = fx
        .conn
        .query("INSERT (:seen {id: 1}) FINISH")
        .expect("a write that says it wants nothing back still writes");
    assert!(rows.columns.is_empty());
    let after = fx
        .conn
        .query("MATCH (s:seen) RETURN count(s) AS n")
        .expect("query");
    assert_eq!(
        after
            .row(0)
            .expect("one row")
            .get_at::<i64>(0)
            .expect("a count"),
        1,
        "the node was inserted"
    );
}

/// A result with no columns is the omitted result of the standard,
/// which is the condition a caller reads to tell it from a result that
/// had columns and no rows in them.
#[test]
fn it_reports_the_omitted_result() {
    let mut fx = Fixture::open("finish-status.zu1");
    let finished = fx.conn.query("MATCH (p:person) FINISH").expect("query");
    assert_eq!(finished.status(), zu::gqlstatus::codes::C00001);
    let empty = fx
        .conn
        .query("MATCH (p:person) FILTER p.id > 1000 RETURN p.id AS id")
        .expect("query");
    assert_eq!(
        empty.status(),
        zu_common::gqlstatus::codes::C00000,
        "a column with no rows under it is still a result"
    );
}
