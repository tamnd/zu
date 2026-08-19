//! Closing a cycle onto an element the pattern already bound.
//!
//! A closing edge pattern is an edge pattern. The engine has three ways
//! of running one, a storage probe, an accumulated edge set and a fused
//! intersection, and all three used to answer whether an edge exists,
//! which is the wrong question: the pattern matches once per edge
//! between the two ends and a pair of nodes can hold several. Two ways a
//! pair holds several are a pattern that names no direction over a pair
//! joined both ways, and parallel edges, and both of them are checked
//! here against a count the unfused walk gives on its own.

use zu::query::{Value, run};
use zu_zu1::file::Zu1File;
use zu_zu1::graph::bulk_load_as;

/// The count one query answers.
fn count(db: &mut Zu1File, source: &str) -> i64 {
    let result = run(source, db, &[]).unwrap_or_else(|e| panic!("{source}: {e}"));
    assert_eq!(result.rows.len(), 1, "{source} did not answer one row");
    match result.rows[0][0] {
        Value::Int(n) => n,
        ref other => panic!("{source} returned {other:?}"),
    }
}

fn graph(dir: &std::path::Path, name: &str, nodes: u32, edges: &[(u32, u32)]) -> Zu1File {
    let mut zu = Zu1File::create(&dir.join(name)).unwrap();
    let mut edges = edges.to_vec();
    edges.sort_unstable();
    bulk_load_as(&mut zu, "node", "edge", u64::from(nodes), &edges).unwrap();
    zu
}

/// A triangle whose every side is stored both ways round, which is what
/// an edge list written in both directions gives.
fn both_ways(dir: &std::path::Path) -> Zu1File {
    graph(
        dir,
        "triangle.zu1",
        3,
        &[(0, 1), (1, 0), (1, 2), (2, 1), (2, 0), (0, 2)],
    )
}

/// The pattern that names no direction takes each stored edge once, so
/// the six edges are twelve matches, one per edge per way round. That
/// number is the one every count below is built out of, and it is
/// answered by a plain walk with nothing to close.
#[test]
fn an_undirected_step_matches_once_per_stored_edge() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = both_ways(dir.path());
    assert_eq!(
        count(
            &mut db,
            "MATCH (a:node)-[:edge]-(b:node) RETURN count(*) AS n"
        ),
        12
    );
}

/// The closing leg counts what the two open legs count. Three distinct
/// nodes take six orderings and each of the three sides has two stored
/// edges to choose from, so the triangle is six times two times two
/// times two.
#[test]
fn an_undirected_cycle_counts_once_per_stored_edge_on_every_side() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = both_ways(dir.path());
    assert_eq!(
        count(
            &mut db,
            "MATCH (a:node)-[:edge]-(b:node)-[:edge]-(c:node)-[:edge]-(a) RETURN count(*) AS n"
        ),
        48
    );
    // The same shape written as two patterns, which is the plan the
    // fused intersection is chosen for rather than the plain close.
    assert_eq!(
        count(
            &mut db,
            "MATCH (a:node)-[:edge]-(b:node)-[:edge]-(c:node), (a)-[:edge]-(c) RETURN count(*) AS n"
        ),
        48
    );
}

/// A closing leg that names a direction takes one of the two stored
/// edges and not both, so the same twenty four open legs close once
/// each. This is the check that the count above is the edges and not a
/// doubling of everything.
#[test]
fn a_directed_close_takes_the_edge_it_names_and_no_other() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = both_ways(dir.path());
    assert_eq!(
        count(
            &mut db,
            "MATCH (a:node)-[:edge]-(b:node)-[:edge]-(c:node), (a)-[:edge]->(c) RETURN count(*) AS n"
        ),
        24
    );
    assert_eq!(
        count(
            &mut db,
            "MATCH (a:node)-[:edge]->(b:node)-[:edge]->(c:node)-[:edge]->(a) RETURN count(*) AS n"
        ),
        6
    );
}

/// Parallel edges are the other way a pair holds more than one edge, and
/// a close counts them the same way an open walk does. Here 0 and 2 are
/// joined by two edges the same way round, so every path that closes on
/// that pair closes twice.
#[test]
fn a_close_counts_parallel_edges_one_at_a_time() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = graph(
        dir.path(),
        "parallel.zu1",
        3,
        &[(0, 1), (1, 2), (0, 2), (0, 2)],
    );
    // The open walk: one row per stored edge, so the doubled pair is
    // two of the four.
    assert_eq!(
        count(
            &mut db,
            "MATCH (a:node)-[:edge]->(b:node) RETURN count(*) AS n"
        ),
        4
    );
    // And the close sees the same two. The only path of two hops is 0
    // to 1 to 2, and it closes on the pair that holds two edges.
    assert_eq!(
        count(
            &mut db,
            "MATCH (a:node)-[:edge]->(b:node)-[:edge]->(c:node), (a)-[:edge]->(c) RETURN count(*) AS n"
        ),
        2
    );
}
