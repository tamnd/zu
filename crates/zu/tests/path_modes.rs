//! The path modes of ISO 16.7: WALK, TRAIL, SIMPLE and ACYCLIC
//! (features G010 to G013).
//!
//! The four differ only in which repeat they forbid, and the pair that
//! is easy to confuse is the last two: ACYCLIC forbids a repeated node,
//! SIMPLE forbids one too except that the path may end where it began.
//! A cycle is therefore a simple path and not an acyclic one, so an
//! engine answering ACYCLIC where SIMPLE was written drops answers
//! rather than refusing the statement, which is what zu did until this
//! file existed.

use zu::Database;
use zu::zu1::file::Zu1File;
use zu::zu1::graph::bulk_load_as;

const NODES: u32 = 5;

/// A triangle with a back edge, a tail, and a shortcut:
///
/// ```text
/// 0 -> 1 -> 2 -> 0        the triangle, so a path from 0 can return
/// 1 -> 0                  a two hop way back, so the shortest cycle is 2
/// 2 -> 3 -> 4             the tail, the only acyclic way to reach 4
/// 0 -> 4                  the shortcut, so 4 is one hop away as well
/// ```
///
/// The shape is chosen so that walking back through the start reaches
/// somewhere: a path 0, 1, 0, 4 exists under WALK, and every mode that
/// forbids a repeated node has to leave it out.
fn seeded(path: &std::path::Path) {
    let mut db = Zu1File::create(path).expect("create");
    let mut edges: Vec<(u32, u32)> = vec![(0, 1), (1, 2), (2, 0), (1, 0), (2, 3), (3, 4), (0, 4)];
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

    /// The element counts of every path the statement answered,
    /// sorted, so a mode is compared by what it found. A path of h hops
    /// holds 2h + 1 elements, alternating node and edge, so one hop is
    /// 3 and four hops is 9.
    fn lengths(&mut self, source: &str) -> Vec<i64> {
        let rows = self.conn.query(source).expect("query");
        let mut out: Vec<i64> = rows
            .iter()
            .map(|row| row.get_by_name::<i64>("len").expect("len"))
            .collect();
        out.sort_unstable();
        out
    }
}

/// Every path from the start back to the start, which is the whole of
/// the difference between the two modes. There are two of them, the
/// back edge at two hops and the triangle at three, and SIMPLE answers
/// both.
#[test]
fn a_simple_path_may_end_where_it_began() {
    let mut fx = Fixture::open("simple-closes.zu1");
    assert_eq!(
        fx.lengths(
            "MATCH p = SIMPLE (a:person {id: 0})-[:knows*1..4]->(b) \
             WHERE b.id = 0 RETURN size(p) AS len"
        ),
        // Two hops and three hops.
        [5, 7]
    );
}

/// The same statement written ACYCLIC, which is what the parser used to
/// tell a writer to use instead. It answers nothing, because a path
/// ending where it began repeats a node, and that is the reason the
/// substitution was a wrong answer rather than a near enough one.
#[test]
fn acyclic_drops_the_paths_simple_keeps() {
    let mut fx = Fixture::open("acyclic-drops.zu1");
    assert!(
        fx.lengths(
            "MATCH p = ACYCLIC (a:person {id: 0})-[:knows*1..4]->(b) \
             WHERE b.id = 0 RETURN size(p) AS len"
        )
        .is_empty()
    );
}

/// The exception is the end of the path and not the middle of it. Node
/// 4 is one hop from the start and four hops around the tail, and a
/// walk that goes back through the start reaches it in three. SIMPLE
/// answers the first two and not the third.
#[test]
fn a_simple_path_may_not_pass_through_where_it_began() {
    let mut fx = Fixture::open("simple-through.zu1");
    let query = "MATCH p = {MODE} (a:person {id: 0})-[:knows*1..4]->(b) \
                 WHERE b.id = 4 RETURN size(p) AS len";
    // One hop and four hops, and nothing at the three hops a walk
    // through the start would take.
    assert_eq!(fx.lengths(&query.replace("{MODE}", "SIMPLE")), [3, 9]);
    let walked = fx.lengths(&query.replace("{MODE}", "WALK"));
    assert!(
        walked.contains(&7),
        "a walk reaches 4 through the start in three hops, got {walked:?}"
    );
}

/// A mode that forbids a repeat needs no upper bound to be finite, and
/// SIMPLE is one of those, so it may be written without one where WALK
/// may not. Seven paths leave node 0 under it and the longest is the
/// four hop tail.
#[test]
fn simple_needs_no_upper_bound() {
    let mut fx = Fixture::open("simple-unbounded.zu1");
    let lengths =
        fx.lengths("MATCH p = SIMPLE (a:person {id: 0})-[:knows*]->(b) RETURN size(p) AS len");
    // Two of one hop, two of two, two of three, one of four.
    assert_eq!(lengths, [3, 3, 5, 5, 7, 7, 9]);
}

/// The mode is in the plan, so a reader can tell which of the four a
/// statement asked for without running it.
#[test]
fn the_plan_names_the_mode() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("simple-plan.zu1");
    seeded(&path);
    let mut session = zu::session::Session::open(&path).expect("open");
    let plan = session
        .explain("MATCH SIMPLE (a:person {id: 0})-[:knows*1..3]->(b) RETURN b.id AS id")
        .expect("a plan");
    assert!(plan.contains("simple"), "{plan}");
}
