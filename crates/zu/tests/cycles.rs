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
//!
//! The last test here is about a second close running under the first,
//! which is what a query joining a cycle to further patterns compiles
//! to, and about the two of them each reading their own answer.

use zu::query::{Value, run};
use zu_zu1::file::Zu1File;
use zu_zu1::graph::{bulk_load_as, bulk_load_keyed};

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

/// The graph the nested close is checked on: a random background so the
/// optimizer is looking at a real degree distribution, and eight nodes
/// wired by hand under it.
///
/// The hand wired part is built so that the two closes stand for
/// different stretches of the id space. Node 1 leaves for 2, 500 and
/// 501, so a close probing it starts at 2; node 0 leaves for 1, 2 and 3,
/// so a close probing that one starts at 1. One step between the two,
/// which is enough: the second close asking about node 4 lands one place
/// along from where the first close recorded node 3, and a run that
/// keeps both answers in one buffer reads that as an edge from 1 to 4
/// that the graph does not hold. The genuine answer goes through node 3
/// instead, which leaves for 2 and 700, and 700 is where the four nodes
/// really do close.
fn nested(dir: &std::path::Path) -> Zu1File {
    const N: u32 = 1000;
    let mut edges: Vec<(u32, u32)> = Vec::new();
    let mut seed = 0x2545_f491_4f6c_dd1d_u64;
    let mut roll = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        10 + (seed % u64::from(N - 10)) as u32
    };
    for from in 10..N {
        for _ in 0..8 {
            let to = roll();
            if to != from {
                edges.push((from, to));
            }
        }
    }
    for to in [1, 2, 3] {
        edges.push((0, to));
    }
    for to in [2, 500, 501] {
        edges.push((1, to));
    }
    for to in [4, 700] {
        edges.push((2, to));
    }
    for to in [2, 700] {
        edges.push((3, to));
    }
    edges.sort_unstable();
    edges.dedup();
    let keys: Vec<u64> = (0..u64::from(N)).collect();
    let mut zu = Zu1File::create(&dir.join("nested.zu1")).unwrap();
    bulk_load_keyed(&mut zu, "node", "edge", u64::from(N), &edges, Some(&keys)).unwrap();
    zu
}

/// A close whose own pipeline closes again. The first one holds what it
/// read of the graph for as long as the walk under it runs, and that
/// walk is where the second one reads its own stretch, so the two are
/// live together and neither may answer out of what the other put down.
///
/// The wedge on its own is the control: two matches, through node 1 and
/// through node 3, and it is the wedge whose close is still open when
/// the second close runs. Adding the fourth node leaves one of the two,
/// the one through node 3, because node 1 has no edge to node 4.
#[test]
fn a_close_under_another_close_answers_out_of_its_own_reading() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = nested(dir.path());
    let wedge = "MATCH (a:node {id: 0})-[:edge]->(b)-[:edge]->(c), (a)-[:edge]->(c) \
                 RETURN b.id AS b, c.id AS c ORDER BY b, c";
    let result = run(wedge, &mut db, &[]).unwrap();
    assert_eq!(
        result.rows,
        vec![
            vec![Value::Int(1), Value::Int(2)],
            vec![Value::Int(3), Value::Int(2)]
        ]
    );

    let closed = "MATCH (a:node {id: 0})-[:edge]->(b)-[:edge]->(c), (a)-[:edge]->(c), \
                  (c)-[:edge]->(d), (b)-[:edge]->(d) \
                  RETURN b.id AS b, c.id AS c, d.id AS d ORDER BY b, c, d";
    let result = run(closed, &mut db, &[]).unwrap();
    assert_eq!(
        result.rows,
        vec![vec![Value::Int(3), Value::Int(2), Value::Int(700)]]
    );
}
