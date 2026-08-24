//! The graph pattern yield clause (ISO 16.14, feature GQ19).
//!
//! `YIELD` says which of the variables a match wrote leave it, so it
//! takes names away where a `LET` adds them. What is checked here is
//! that the names it does not carry are gone, that the names it does
//! carry are the same values under whatever name it gave them, and
//! that narrowing the columns is not grouping the rows: a yield answers
//! the rows the match answered.
//!
//! Two more shapes are here. `YIELD NO BINDINGS` is the other way ISO
//! writes the item list, and it takes every name away rather than some
//! of them, the rows staying where they are. And ISO 9.2 puts a yield
//! behind a `NEXT` (GQ20), where the names it takes are the columns the
//! statement in front returned rather than the variables a match wrote,
//! which is the same clause reading the same scope.

use zu::Database;
use zu::zu1::file::Zu1File;
use zu::zu1::graph::bulk_load_as;

const NODES: u32 = 5;

/// A ring of five, and two more edges into 2, so seven matches lead to
/// five distinct ends. That gap is what tells a yield from a grouping.
fn seeded(path: &std::path::Path) {
    let mut db = Zu1File::create(path).expect("create");
    let mut edges: Vec<(u32, u32)> = (0..NODES).map(|i| (i, (i + 1) % NODES)).collect();
    edges.push((0, 2));
    edges.push((3, 2));
    edges.sort_unstable();
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
}

/// The clause narrows the columns and leaves the rows alone, so the
/// seven matches are still seven rows after a yield that keeps one end
/// of them, even though five values is all those seven rows hold.
#[test]
fn a_yield_keeps_every_row_the_match_answered() {
    let mut fx = Fixture::open("yield-rows.zu1");
    assert_eq!(
        fx.one("MATCH (p:person)-[:knows]->(q:person) YIELD q RETURN count(*) AS n"),
        7
    );
    assert_eq!(
        fx.one("MATCH (p:person)-[:knows]->(q:person) RETURN count(*) AS n"),
        7,
        "the yield changed nothing about how many rows there are"
    );
    assert_eq!(
        fx.one("MATCH (p:person)-[:knows]->(q:person) YIELD q RETURN count(DISTINCT q.id) AS n"),
        5,
        "five values in those seven rows, which is what a grouping would have answered"
    );
}

/// A name the yield did not carry is not a name the clause after it
/// can read, which is the whole point of writing one.
#[test]
fn a_name_the_yield_dropped_is_gone() {
    let mut fx = Fixture::open("yield-drop.zu1");
    let err = fx
        .conn
        .query("MATCH (p:person)-[:knows]->(q:person) YIELD q RETURN p.id AS id")
        .expect_err("p did not leave the match");
    assert!(
        err.to_string().contains("'p' is not defined"),
        "{err}, want the name"
    );
}

/// The values carried are the ones the match matched, under the name
/// the yield gave them.
#[test]
fn a_yield_may_rename_what_it_carries() {
    let mut fx = Fixture::open("yield-rename.zu1");
    let rows = fx
        .conn
        .query(
            "MATCH (p:person)-[:knows]->(q:person) \
             WHERE p.id = 0 \
             YIELD q AS friend \
             RETURN friend.id AS id",
        )
        .expect("query");
    let mut ids: Vec<i64> = rows
        .iter()
        .map(|row| row.get_by_name::<i64>("id").expect("id"))
        .collect();
    ids.sort_unstable();
    assert_eq!(ids, [1, 2], "0 knows 1 and 2");
}

/// A yield names variables rather than computing values, so a name the
/// match did not write is refused by name.
#[test]
fn a_name_the_match_did_not_write_is_refused() {
    let mut fx = Fixture::open("yield-unknown.zu1");
    let err = fx
        .conn
        .query("MATCH (p:person)-[:knows]->(q:person) YIELD r RETURN r.id AS id")
        .expect_err("r is nobody");
    assert!(
        err.to_string().contains("yield a name the match wrote"),
        "{err}, want what to do about it"
    );
}

/// Two items ending under one name would leave the clause after this
/// one two things to read for it, so it is refused with the way out.
#[test]
fn yielding_one_name_twice_is_refused() {
    let mut fx = Fixture::open("yield-twice.zu1");
    let err = fx
        .conn
        .query("MATCH (p:person)-[:knows]->(q:person) YIELD p, q AS p RETURN p.id AS id")
        .expect_err("two p");
    assert!(
        err.to_string().contains("names 'p' twice"),
        "{err}, want the name"
    );
}

/// `NO BINDINGS` is the other alternative of the item list, and what it
/// says is that the match ran and nothing it wrote is in scope behind
/// it. A binding table is a multiset of records, so a record with no
/// fields is still a record and the seven matches are still seven rows.
#[test]
fn no_bindings_keeps_every_row_and_lets_nothing_out() {
    let mut fx = Fixture::open("yield-no-bindings.zu1");
    assert_eq!(
        fx.one("MATCH (p:person)-[:knows]->(q:person) YIELD NO BINDINGS RETURN count(*) AS n"),
        7
    );
    for source in [
        "MATCH (p:person)-[:knows]->(q:person) YIELD NO BINDINGS RETURN q.id AS n",
        "MATCH (p:person)-[:knows]->(q:person) YIELD NO BINDINGS RETURN p.id AS n",
    ] {
        let err = fx.conn.query(source).expect_err("nothing left the match");
        assert!(err.to_string().contains("is not defined"), "{err}");
    }
}

/// `no` is a name a query may write, so both words are read before
/// either is taken and `YIELD no` is the variable it looks like.
#[test]
fn a_variable_named_no_is_not_the_first_half_of_no_bindings() {
    let mut fx = Fixture::open("yield-no-name.zu1");
    assert_eq!(
        fx.one("MATCH (no:person) YIELD no RETURN count(no) AS n"),
        5
    );
}

/// ISO 9.2 puts a yield behind a `NEXT`, which is the one place in the
/// language one may be written without a procedure name in front of it.
/// The whole binding table is handed on unless it narrows it, so the
/// column it kept is there under the name it gave and the one it left
/// behind is gone.
#[test]
fn a_yield_behind_a_next_narrows_what_the_statement_past_it_reads() {
    let mut fx = Fixture::open("yield-next.zu1");
    assert_eq!(
        fx.one(
            "MATCH (p:person) RETURN p.id AS id, p.id + 1 AS bump \
             NEXT YIELD id AS mine \
             RETURN count(mine) AS n"
        ),
        5
    );
    assert_eq!(
        fx.one(
            "MATCH (p:person) RETURN p.id AS id, p.id + 1 AS bump \
             NEXT RETURN sum(bump) AS n"
        ),
        15,
        "the whole table is handed on where no yield narrows it"
    );
    let err = fx
        .conn
        .query(
            "MATCH (p:person) RETURN p.id AS id, p.id + 1 AS bump \
             NEXT YIELD id \
             RETURN sum(bump) AS n",
        )
        .expect_err("bump did not come through the yield");
    assert!(err.to_string().contains("'bump' is not defined"), "{err}");
}

/// A yield behind a `NEXT` names columns the statement in front
/// returned, so a name it never returned is refused the way a name a
/// match never wrote is.
#[test]
fn a_yield_behind_a_next_cannot_name_a_column_nobody_returned() {
    let mut fx = Fixture::open("yield-next-unknown.zu1");
    let err = fx
        .conn
        .query("MATCH (p:person) RETURN p.id AS id NEXT YIELD other RETURN other AS n")
        .expect_err("other is nobody");
    assert!(
        err.to_string().contains("yield a name the match wrote"),
        "{err}, want what to do about it"
    );
}

/// The clause belongs to the match in front of it, so a second match
/// after a yield is read against the names the yield left.
#[test]
fn what_follows_a_yield_reads_what_it_left() {
    let mut fx = Fixture::open("yield-chain.zu1");
    let rows = fx
        .conn
        .query(
            "MATCH (p:person)-[:knows]->(q:person) \
             WHERE p.id = 0 \
             YIELD q \
             MATCH (q)-[:knows]->(r:person) \
             RETURN r.id AS id",
        )
        .expect("query");
    let mut ids: Vec<i64> = rows
        .iter()
        .map(|row| row.get_by_name::<i64>("id").expect("id"))
        .collect();
    ids.sort_unstable();
    assert_eq!(ids, [2, 3], "1 knows 2, and 2 knows 3");
}
