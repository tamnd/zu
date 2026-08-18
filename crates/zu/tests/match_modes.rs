//! The match modes of ISO 16.9: DIFFERENT EDGES and REPEATABLE
//! ELEMENTS (features G002 and G003).
//!
//! A path mode speaks about one path and a match mode speaks about the
//! list of patterns a match statement writes. DIFFERENT EDGES is what a
//! list that names no mode means, and it says that no edge answers two
//! of the edge patterns of the list at once, so `(a)-[e]->(b),
//! (a)-[f]->(b)` answers the pairs of parallel edges rather than every
//! edge paired with itself. REPEATABLE ELEMENTS lifts that and lets a
//! pattern take whatever fits.

use zu::Database;
use zu::zu1::file::Zu1File;
use zu::zu1::graph::bulk_load_as;

const NODES: u32 = 3;

/// A self loop, a pair of parallel edges, and a way back:
///
/// ```text
/// 0 -> 0        the self loop, so a two hop walk may take one edge twice
/// 0 -> 1        twice over, so a pair of patterns has two edges to tell apart
/// 1 -> 0        the way back
/// 1 -> 2        a tail, so not every edge is part of a cycle
/// ```
fn seeded(path: &std::path::Path) {
    let mut db = Zu1File::create(path).expect("create");
    let mut edges: Vec<(u32, u32)> = vec![(0, 0), (0, 1), (0, 1), (1, 0), (1, 2)];
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

    /// How many rows the statement answered, which is what a match mode
    /// changes: the same patterns over the same graph, and more or
    /// fewer ways to fill them.
    fn count(&mut self, source: &str) -> i64 {
        let rows = self.conn.query(source).expect("query");
        rows.iter()
            .map(|row| row.get_by_name::<i64>("n").expect("n"))
            .next()
            .expect("a row")
    }

    /// How many rows the statement answered, for the cases where what
    /// is being counted is the bindings themselves.
    fn rows(&mut self, source: &str) -> usize {
        self.conn.query(source).expect("query").iter().count()
    }

    /// What the engine said about a statement it refused.
    fn refusal(&mut self, source: &str) -> String {
        match self.conn.query(source) {
            Ok(_) => panic!("the statement was accepted"),
            Err(error) => error.to_string(),
        }
    }
}

/// Two patterns joining the same pair of nodes. The graph holds one
/// pair of parallel edges, so DIFFERENT EDGES answers the two ways of
/// telling those two apart and nothing else, while REPEATABLE ELEMENTS
/// answers those plus every edge paired with itself.
#[test]
fn two_patterns_over_one_pair_of_nodes() {
    let mut fx = Fixture::open("parallel.zu1");
    let source = "MATCH (a:person)-[e:knows]->(b:person), (a)-[f:knows]->(b) RETURN COUNT(*) AS n";
    // The default is DIFFERENT EDGES, so writing it says what the
    // statement above already meant.
    assert_eq!(fx.count(source), 2);
    let source = "MATCH DIFFERENT EDGES (a:person)-[e:knows]->(b:person), (a)-[f:knows]->(b) \
                  RETURN COUNT(*) AS n";
    assert_eq!(fx.count(source), 2);
    let source = "MATCH REPEATABLE ELEMENTS (a:person)-[e:knows]->(b:person), (a)-[f:knows]->(b) \
                  RETURN e, f";
    assert_eq!(fx.rows(source), 7);
}

/// Two steps of one pattern, which can only take the same edge where
/// the graph holds a loop and both ends land on it. That pair is not
/// checked: the test would run on every two hop walk of every query to
/// rule out a shape no graph without a loop holds, so it waits for the
/// engine to know which tables hold one. The self loop in this fixture
/// is what the two counts would differ by.
#[test]
fn two_steps_of_one_pattern() {
    let mut fx = Fixture::open("steps.zu1");
    let source = "MATCH (a:person)-[e:knows]->(b:person)-[f:knows]->(c:person) \
                  RETURN COUNT(*) AS n";
    assert_eq!(fx.count(source), 10);
    // Nine of those ten walks take two edges. Written out, the tenth is
    // the one the standard leaves out and this answers.
    let source = "MATCH (a:person)-[e:knows]->(b:person)-[f:knows]->(c:person) \
                  WHERE e <> f RETURN COUNT(*) AS n";
    assert_eq!(fx.count(source), 9);
}

/// A fixed step beside a variable-length one. What the walk took is a
/// list of edges, so the rule is that the fixed step's edge is none of
/// them.
#[test]
fn a_step_beside_a_walk() {
    let mut fx = Fixture::open("walk.zu1");
    let source = "MATCH (a:person)-[e:knows]->(b:person), (a)-[es:knows*1..2]->(c:person) \
                  RETURN COUNT(*) AS n";
    let different = fx.count(source);
    let source = "MATCH REPEATABLE ELEMENTS (a:person)-[e:knows]->(b:person), \
                  (a)-[es:knows*1..2]->(c:person) RETURN COUNT(*) AS n";
    assert!(
        different < fx.count(source),
        "the walk may retake the edge the step took only under REPEATABLE ELEMENTS"
    );
}

/// Steps that name no type in common. No edge answers both whatever the
/// graph holds, so the mode makes no difference to what is found.
#[test]
fn steps_naming_different_types() {
    let mut fx = Fixture::open("types.zu1");
    let source = "MATCH (a:person)-[e:knows]->(b:person), (a)-[f:likes]->(b) RETURN COUNT(*) AS n";
    assert_eq!(fx.count(source), 0);
}

/// The four spellings of each mode. `EDGE` and `EDGES` are the same
/// word, and `BINDINGS` after it is the standard saying that what may
/// not repeat is what the patterns bound.
#[test]
fn the_spellings_of_a_match_mode() {
    let mut fx = Fixture::open("spellings.zu1");
    let patterns = "(a:person)-[e:knows]->(b:person), (a)-[f:knows]->(b) RETURN e, f";
    for mode in [
        "DIFFERENT EDGES",
        "DIFFERENT EDGE",
        "DIFFERENT EDGE BINDINGS",
        "DIFFERENT EDGES BINDINGS",
    ] {
        assert_eq!(fx.rows(&format!("MATCH {mode} {patterns}")), 2, "{mode}");
    }
    for mode in [
        "REPEATABLE ELEMENTS",
        "REPEATABLE ELEMENT",
        "REPEATABLE ELEMENT BINDINGS",
        "REPEATABLE ELEMENTS BINDINGS",
    ] {
        assert_eq!(fx.rows(&format!("MATCH {mode} {patterns}")), 7, "{mode}");
    }
}

/// A match mode settles the path mode a pattern that names none walks
/// under, so an unbounded pattern under REPEATABLE ELEMENTS is a walk
/// with no end and is refused where the same pattern under the default
/// is a trail and runs.
#[test]
fn the_match_mode_settles_the_path_mode() {
    let mut fx = Fixture::open("unbounded.zu1");
    let source = "MATCH (a:person)-[:knows*]->(b:person) RETURN COUNT(*) AS n";
    assert!(fx.count(source) > 0);
    let source = "MATCH REPEATABLE ELEMENTS (a:person)-[:knows*]->(b:person) RETURN COUNT(*) AS n";
    let refusal = fx.refusal(source);
    assert!(refusal.contains("unbounded WALK"), "got: {refusal}");
    // A pattern naming its own mode keeps it: the match mode fills in
    // what a pattern left out rather than overruling what it wrote.
    let source = "MATCH REPEATABLE ELEMENTS TRAIL (a:person)-[:knows*]->(b:person) \
                  RETURN COUNT(*) AS n";
    assert!(fx.count(source) > 0);
}

/// A match statement block gathers the pattern lists of several
/// statements, and each of them keeps its own edges apart: two patterns
/// of one statement may not take the same edge, two of two statements
/// may.
#[test]
fn a_block_keeps_its_lists_apart() {
    let mut fx = Fixture::open("block.zu1");
    // Seven pairs, taking each edge with itself and the parallel pair
    // both ways round, and one row for the node with no edge at all,
    // which is what the OPTIONAL is here for.
    let source = "MATCH (a:person) OPTIONAL { MATCH (a)-[e:knows]->(b:person) \
                  MATCH (a)-[f:knows]->(b) } RETURN COUNT(*) AS n";
    assert_eq!(fx.count(source), 8);
    // The same two patterns in one statement are one list, and then the
    // edges have to differ: the parallel pair both ways round, and a
    // row for each of the three nodes that answered nothing.
    let source = "MATCH (a:person) OPTIONAL { MATCH (a)-[e:knows]->(b:person), (a)-[f:knows]->(b) } \
                  RETURN COUNT(*) AS n";
    assert_eq!(fx.count(source), 4);
}
