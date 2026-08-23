//! The reachability rewrite: a variable length step whose paths the
//! query throws away walks each node once instead of once per path.
//!
//! The rewrite turns on when nothing reads the rel slot and the answer
//! is a set, and the interesting half of that second condition is what
//! a `LIMIT` does to it. A `LIMIT` reads an order, and the order rows
//! with equal sort keys arrive in is the one thing the rewrite changes,
//! so it is refused unless the sort keys tell every row of the set
//! apart. That is the shape the LDBC neighbourhood queries are written
//! in, three hops of `KNOWS` with a `DISTINCT` and a tie broken on the
//! person id, and it is the difference between a walk over the
//! reachable set and a walk over every path through it.

use zu::query::{Value, explain_analyze, run};
use zu_zu1::file::Zu1File;
use zu_zu1::graph::bulk_load_as;

const NODES: u32 = 24;

/// A ring with every third node also joined to the one six along, so a
/// node three hops out is reachable by more than one walk and the two
/// numbers, paths and nodes, are not the same number.
fn graph(dir: &std::path::Path) -> Zu1File {
    let mut db = Zu1File::create(&dir.join("reach.zu1")).unwrap();
    let mut edges: Vec<(u32, u32)> = (0..NODES).map(|i| (i, (i + 1) % NODES)).collect();
    edges.extend((0..NODES).step_by(3).map(|i| (i, (i + 6) % NODES)));
    edges.sort_unstable();
    edges.dedup();
    bulk_load_as(&mut db, "person", "knows", NODES.into(), &edges).unwrap();
    db
}

fn ids(db: &mut Zu1File, source: &str) -> Vec<i64> {
    run(source, db, &[])
        .unwrap_or_else(|e| panic!("{source}: {e}"))
        .rows
        .iter()
        .map(|row| match row[0] {
            Value::Int(i) => i,
            ref other => panic!("{source} answered {other:?} where an id was due"),
        })
        .collect()
}

fn walks(db: &mut Zu1File, source: &str) -> bool {
    let text = explain_analyze(source, db, &[]).unwrap_or_else(|e| panic!("{source}: {e}"));
    assert!(text.contains("VarExpand"), "{source} did not walk: {text}");
    text.contains("reach")
}

/// The LDBC shape: a bounded neighbourhood, a `DISTINCT` over two
/// columns, and an order that names both of them before the page is
/// taken. Two rows of the set differ somewhere, and everywhere they can
/// differ is a sort key, so the page is the same page whichever order
/// the rows were found in and the walk is free to find each node once.
#[test]
fn a_page_of_a_fully_ordered_set_still_walks_the_reachable_set() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = graph(dir.path());
    let source = "MATCH (a:person)-[:knows*1..3]->(b:person) WHERE a.id = 0 \
                  RETURN DISTINCT b.id AS id, b.id % 4 AS bucket \
                  ORDER BY bucket ASC, id ASC LIMIT 5";
    assert!(walks(&mut db, source), "the page did not take the walk");

    // And it is the page the enumeration would have handed up: the
    // same query without the page, cut to the same length.
    let whole = ids(
        &mut db,
        "MATCH (a:person)-[:knows*1..3]->(b:person) WHERE a.id = 0 \
         RETURN DISTINCT b.id AS id, b.id % 4 AS bucket \
         ORDER BY bucket ASC, id ASC",
    );
    assert!(whole.len() > 5, "the fixture reaches enough: {whole:?}");
    assert_eq!(ids(&mut db, source), whole[..5], "the page moved");
}

/// The same query with one of its columns left out of the order. Rows
/// sharing a bucket are then in the order they were found, which is the
/// one thing the rewrite is allowed to change, so it stays off and the
/// paths enumerate.
#[test]
fn a_page_of_a_partly_ordered_set_enumerates() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = graph(dir.path());
    let source = "MATCH (a:person)-[:knows*1..3]->(b:person) WHERE a.id = 0 \
                  RETURN DISTINCT b.id AS id, b.id % 4 AS bucket \
                  ORDER BY bucket ASC LIMIT 5";
    assert!(
        !walks(&mut db, source),
        "the tie was decided by the rewrite"
    );
}

/// A `DISTINCT` with no page at all keeps the walk it always had, page
/// or no page, which is what says the rule above narrowed nothing.
#[test]
fn a_set_with_no_page_walks_the_reachable_set() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = graph(dir.path());
    assert!(walks(
        &mut db,
        "MATCH (a:person)-[:knows*1..3]->(b:person) WHERE a.id = 0 \
         RETURN DISTINCT b.id AS id ORDER BY id ASC"
    ));
}

/// A page taken with no order under it reads the order the rows arrive
/// in and nothing else, so there is nothing to settle and the rewrite
/// stays off.
#[test]
fn a_page_with_no_order_enumerates() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = graph(dir.path());
    assert!(!walks(
        &mut db,
        "MATCH (a:person)-[:knows*1..3]->(b:person) WHERE a.id = 0 \
         RETURN DISTINCT b.id AS id LIMIT 5"
    ));
}

/// The walk and the enumeration answer the same set, which is the
/// premise the whole rewrite rests on. The one is asked for with a
/// `DISTINCT` the rewrite can read and the other with a page it
/// cannot, and both are sorted here rather than compared as they came.
#[test]
fn the_walk_reaches_what_the_enumeration_reaches() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = graph(dir.path());
    let walked = ids(
        &mut db,
        "MATCH (a:person)-[:knows*1..3]->(b:person) WHERE a.id = 0 \
         RETURN DISTINCT b.id AS id ORDER BY id ASC",
    );
    let mut enumerated = ids(
        &mut db,
        "MATCH (a:person)-[r:knows*1..3]->(b:person) WHERE a.id = 0 \
         RETURN b.id AS id, size(r) AS hops ORDER BY id ASC",
    );
    enumerated.dedup();
    assert_eq!(walked, enumerated, "the two disagree on who is reachable");
    assert!(!walked.is_empty(), "the fixture reaches nobody");
}
