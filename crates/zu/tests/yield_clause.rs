//! The graph pattern yield clause (ISO 16.14, feature GQ19).
//!
//! `YIELD` says which of the variables a match wrote leave it, so it
//! takes names away where a `LET` adds them. What is checked here is
//! that the names it does not carry are gone, that the names it does
//! carry are the same values under whatever name it gave them, and
//! that narrowing the columns is not grouping the rows: a yield answers
//! the rows the match answered.

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
