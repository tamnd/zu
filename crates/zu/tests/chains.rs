//! `NEXT`, which is how GQL writes one query out of several statements
//! (ISO 12.2, feature GQ20).
//!
//! The unit tests in `zu_query` check that a chain parses into
//! statements and plans into one pipeline. What is checked here is that
//! it answers: three statements over a real store, each one reading the
//! result the one before it returned, and nothing else of it.

use zu::Database;
use zu::zu1::file::Zu1File;
use zu::zu1::graph::bulk_load_as;

const NODES: u32 = 200;

fn seeded(path: &std::path::Path) {
    let mut db = Zu1File::create(path).expect("create");
    let mut edges: Vec<(u32, u32)> = (0..NODES)
        .flat_map(|i| [(i, (i + 1) % NODES), (i, (i + 7) % NODES)])
        .collect();
    edges.sort_unstable();
    bulk_load_as(&mut db, "person", "knows", NODES.into(), &edges).expect("load");
}

/// The chain and the same question written with WITH answer the same
/// rows, which is what says the composition is a way of writing a query
/// rather than a different query.
#[test]
fn a_chain_of_three_statements_answers_what_it_ends_with() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("chain.zu1");
    seeded(&path);
    let db = Database::open(&path).expect("open");
    let mut conn = db.connect().expect("connect");

    let mut ids = |source: &str| {
        let rows = conn.query(source).expect("query");
        rows.iter()
            .map(|row| row.get_by_name::<i64>("id").expect("an integer column"))
            .collect::<Vec<i64>>()
    };
    let chained = ids("MATCH (p:person) WHERE p.id < 3 RETURN p AS p \
         NEXT MATCH (p)-[:knows]->(f) RETURN f AS f \
         NEXT RETURN f.id AS id ORDER BY id");
    assert_eq!(chained, [1, 2, 3, 7, 8, 9]);
    let withs = ids("MATCH (p:person) WHERE p.id < 3 WITH p AS p \
         MATCH (p)-[:knows]->(f) WITH f AS f \
         RETURN f.id AS id ORDER BY id");
    assert_eq!(chained, withs);
}

/// The shape the conformance corpus asks GQ20 for: a statement that
/// answers rows, and a statement behind the NEXT that counts them.
#[test]
fn a_chain_may_count_what_the_statement_in_front_of_it_answered() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("count.zu1");
    seeded(&path);
    let db = Database::open(&path).expect("open");
    let mut conn = db.connect().expect("connect");

    let rows = conn
        .query("MATCH (p:person) WHERE p.id < 4 RETURN p.id AS id NEXT RETURN count(*) AS n")
        .expect("query");
    let counted: Vec<i64> = rows
        .iter()
        .map(|row| row.get_by_name::<i64>("n").expect("an integer column"))
        .collect();
    assert_eq!(counted, [4]);
}

/// What a statement hands the one behind it is its result table. The
/// variables it matched to build that table are not part of it, so
/// reading one is the same error as reading a name nobody bound.
#[test]
fn a_chain_hands_over_its_result_and_nothing_else() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("scope.zu1");
    seeded(&path);
    let db = Database::open(&path).expect("open");
    let mut conn = db.connect().expect("connect");

    let err = conn
        .query("MATCH (p:person) RETURN p.id AS id NEXT RETURN p.id AS again")
        .expect_err("p is gone");
    assert!(
        err.to_string().contains("'p' is not defined"),
        "{err}, want the undefined variable"
    );
}

/// A write projects nothing and still stands in a chain: what it hands
/// over is what it was given, so the statement behind it reads the
/// graph the write left.
#[test]
fn a_write_may_stand_in_a_chain() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("write.zu1");
    seeded(&path);
    let db = Database::open(&path).expect("open");
    let mut conn = db.connect().expect("connect");

    let rows = conn
        .query(
            "MATCH (n:person) WHERE n.id = 0 DETACH DELETE n \
             NEXT MATCH (m:person) WHERE m.id = 1 RETURN m.id AS id",
        )
        .expect("query");
    let ids: Vec<i64> = rows
        .iter()
        .map(|row| row.get_by_name::<i64>("id").expect("an integer column"))
        .collect();
    assert_eq!(ids, [1], "the statement behind the NEXT answered");
    let gone = conn
        .query("MATCH (n:person) WHERE n.id = 0 RETURN n.id AS id")
        .expect("query");
    assert_eq!(
        gone.iter().count(),
        0,
        "the delete in front of the NEXT happened"
    );
}

/// What the chain costs, read off the plan rather than the clock. Three
/// statements joined by NEXT plan into one pipeline with no operator
/// between the statements to hold a result table: the listing is the
/// listing of the same question written with WITH, operator for
/// operator, and a chain that materialised between its statements
/// could not be.
#[test]
fn a_chain_plans_as_one_pipeline() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("fuse.zu1");
    seeded(&path);
    let mut session = zu::session::Session::open(&path).expect("open");
    let chained = session
        .explain(
            "MATCH (p:person) WHERE p.id < 3 RETURN p AS p \
             NEXT MATCH (p)-[:knows]->(f) RETURN f AS f \
             NEXT RETURN f.id AS id ORDER BY id",
        )
        .expect("a plan");
    let withs = session
        .explain(
            "MATCH (p:person) WHERE p.id < 3 WITH p AS p \
             MATCH (p)-[:knows]->(f) WITH f AS f \
             RETURN f.id AS id ORDER BY id",
        )
        .expect("a plan");
    assert_eq!(chained, withs, "the chain plans differently from the WITH");
    assert_eq!(
        chained.lines().filter(|l| l.contains("ScanNodes")).count(),
        1,
        "one scan drives the whole chain, got:\n{chained}"
    );
}
