//! `MERGE`, the statement that finds a pattern or writes it.
//!
//! It is Cypher's word and GQL has none for it, and it is the last of
//! the write words this engine did not answer. What it means is a walk
//! and a write over one pattern: the walk decides, the write runs for
//! the rows the walk found nothing for, and the row on the other side
//! holds the element either way, so a clause behind it reads what was
//! found and what was made without knowing which is which.
//!
//! What is checked here is that both halves happen, that they happen to
//! the right rows, that one statement merging the same thing twice
//! writes it once, and that the patterns a merge cannot mean are turned
//! away rather than half run.

use zu::Database;
use zu::query::Value;
use zu::zu1::file::Zu1File;
use zu::zu1::graph::bulk_load_as;
use zu::zu1::props::{PropValues, store_props};

const NODES: u64 = 4;

struct Fixture {
    _dir: tempfile::TempDir,
    conn: zu::Connection,
}

impl Fixture {
    fn open(name: &str) -> Fixture {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(name);
        let mut db = Zu1File::create(&path).expect("create");
        let edges: Vec<(u32, u32)> = vec![(0, 1)];
        bulk_load_as(&mut db, "person", "knows", NODES, &edges).expect("load");
        let names: Vec<Vec<u8>> = ["ada", "amy", "joe", "zoe"]
            .iter()
            .map(|n| n.as_bytes().to_vec())
            .collect();
        let refs: Vec<&[u8]> = names.iter().map(Vec::as_slice).collect();
        let ages: Vec<u64> = (0..NODES).map(|i| 10 + i).collect();
        store_props(
            &mut db,
            "person",
            &[
                ("age", PropValues::Int(&ages)),
                ("name", PropValues::Str(&refs)),
            ],
        )
        .expect("props");
        drop(db);
        let db = Database::open(&path).expect("open");
        let conn = db.connect().expect("connect");
        Fixture { _dir: dir, conn }
    }

    fn run(&mut self, text: &str) -> Vec<Vec<Value>> {
        self.conn.query(text).expect(text).rows.into_vec()
    }

    /// How many rows the person table holds, which is what says whether
    /// a merge wrote one.
    fn people(&mut self) -> i64 {
        let rows = self.run("MATCH (p:person) RETURN count(p) AS n");
        match rows[0][0] {
            Value::Int(n) => n,
            ref other => panic!("count answered {other:?}"),
        }
    }
}

/// A pattern that is already there is found, and nothing is written.
#[test]
fn a_pattern_that_is_there_is_found() {
    let mut fx = Fixture::open("merge-found.zu1");
    let rows = fx.run("MERGE (p:person {age: 11}) RETURN p.name AS name");
    assert_eq!(rows.len(), 1, "one row, the one that was there");
    assert_eq!(rows[0][0], Value::Str("amy".into()));
    assert_eq!(fx.people(), NODES as i64, "a merge that found wrote a row");
}

/// A pattern that is not there is written, with the properties the
/// pattern named, and the row on the other side holds it.
#[test]
fn a_pattern_that_is_not_there_is_written() {
    let mut fx = Fixture::open("merge-written.zu1");
    let rows = fx.run("MERGE (p:person {age: 99, name: 'eva'}) RETURN p.name AS name");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], Value::Str("eva".into()));
    assert_eq!(fx.people(), NODES as i64 + 1);
    let again = fx.run("MATCH (p:person) WHERE p.age = 99 RETURN p.name AS name");
    assert_eq!(again.len(), 1, "the row is in the store");
    assert_eq!(again[0][0], Value::Str("eva".into()));
}

/// The second time the same statement runs it finds what the first one
/// wrote, which is the whole point of the word.
#[test]
fn the_same_merge_twice_writes_once() {
    let mut fx = Fixture::open("merge-twice.zu1");
    let text = "MERGE (p:person {age: 99, name: 'eva'}) RETURN p.age AS age";
    let first = fx.run(text);
    let second = fx.run(text);
    assert_eq!(first, second, "the second run found the first one's row");
    assert_eq!(fx.people(), NODES as i64 + 1);
}

/// One statement merging the same thing twice writes it once. The walk
/// answers both rows against the store as it was when the statement
/// started, so nothing but the statement itself can tell the second row
/// that the first one wrote what it is asking for.
#[test]
fn one_statement_merging_the_same_thing_twice_writes_it_once() {
    let mut fx = Fixture::open("merge-dedup.zu1");
    let rows = fx
        .run("UNWIND [99, 99, 98] AS a MERGE (p:person {age: a, name: 'eva'}) RETURN p.age AS age");
    assert_eq!(rows.len(), 3, "a row per row that ran");
    assert_eq!(fx.people(), NODES as i64 + 2, "two ages, two rows");
    assert_eq!(rows[0], rows[1], "the same element both times");
}

/// `ON CREATE SET` writes the rows the pattern was written for and
/// `ON MATCH SET` the rows it was found for, so a statement that does
/// both to a mix of rows covers every row once.
#[test]
fn on_create_and_on_match_split_the_rows_between_them() {
    let mut fx = Fixture::open("merge-on.zu1");
    let rows = fx.run(
        "UNWIND [11, 99] AS a MERGE (p:person {age: a}) \
         ON CREATE SET p.name = 'made' \
         ON MATCH SET p.name = 'found' \
         RETURN p.age AS age",
    );
    assert_eq!(rows.len(), 2);
    let names = fx.run("MATCH (p:person) WHERE p.age = 11 OR p.age = 99 RETURN p.name AS name");
    let mut got: Vec<String> = names
        .iter()
        .map(|row| match &row[0] {
            Value::Str(name) => name.clone(),
            other => panic!("name answered {other:?}"),
        })
        .collect();
    got.sort();
    assert_eq!(got, ["found", "made"]);
}

/// An edge between two elements an earlier clause found is written when
/// it is not there and found when it is, and the ends stay where they
/// were either way.
#[test]
fn an_edge_is_merged_between_ends_that_were_found() {
    let mut fx = Fixture::open("merge-edge.zu1");
    let text = "MATCH (a:person), (b:person) WHERE a.age = 12 AND b.age = 13 \
                MERGE (a)-[r:knows]->(b) RETURN count(r) AS n";
    let first = fx.run(text);
    assert_eq!(first[0][0], Value::Int(1));
    let edges = fx.run("MATCH (a:person)-[:knows]->(b:person) RETURN count(a) AS n");
    assert_eq!(
        edges[0][0],
        Value::Int(2),
        "the loaded edge and the new one"
    );
    let second = fx.run(text);
    assert_eq!(second[0][0], Value::Int(1));
    let again = fx.run("MATCH (a:person)-[:knows]->(b:person) RETURN count(a) AS n");
    assert_eq!(again[0][0], Value::Int(2), "the second run wrote an edge");
}

/// An element with no label says nothing about which table it goes in,
/// and a merge has to be able to write it.
#[test]
fn an_element_with_no_label_is_refused() {
    let mut fx = Fixture::open("merge-nolabel.zu1");
    let err = fx.conn.query("MERGE (p)").expect_err("no table named");
    assert!(err.to_string().contains("MERGE"), "{err}");
}

/// A pattern that names only elements earlier clauses found has nothing
/// to look for and nothing to write.
#[test]
fn a_pattern_with_nothing_to_write_is_refused() {
    let mut fx = Fixture::open("merge-nothing.zu1");
    let err = fx
        .conn
        .query("MATCH (p:person) MERGE (p)")
        .expect_err("nothing to merge");
    assert!(err.to_string().contains("already found"), "{err}");
}

/// `ON CREATE SET` writes what the pattern wrote. An element the merge
/// was handed is not one it makes, so there is no row it would run for.
#[test]
fn on_create_writing_an_element_it_was_given_is_refused() {
    let mut fx = Fixture::open("merge-oncreate.zu1");
    let err = fx
        .conn
        .query("MATCH (a:person) MERGE (a)-[r:knows]->(b:person {age: 99}) ON CREATE SET a.age = 1")
        .expect_err("a is not made here");
    assert!(
        err.to_string().contains("rather than one it makes"),
        "{err}"
    );
}

/// The value an `ON CREATE SET` writes is worked out before the element
/// is made, so it cannot read the element.
#[test]
fn on_create_reading_the_element_it_makes_is_refused() {
    let mut fx = Fixture::open("merge-oncreate-reads.zu1");
    let err = fx
        .conn
        .query("MERGE (p:person {age: 99}) ON CREATE SET p.name = p.name")
        .expect_err("p holds nothing yet");
    assert!(err.to_string().contains("not implemented yet"), "{err}");
}

/// A name the pattern was handed carries nothing of its own, because a
/// label or a property on it would be describing an element that has
/// already been described.
#[test]
fn a_label_on_an_element_it_was_given_is_refused() {
    let mut fx = Fixture::open("merge-relabel.zu1");
    let err = fx
        .conn
        .query("MATCH (a:person) MERGE (a:person)-[r:knows]->(b:person {age: 99})")
        .expect_err("a is already described");
    assert!(err.to_string().contains("already stands for"), "{err}");
}
