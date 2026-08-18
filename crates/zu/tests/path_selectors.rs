//! The path selectors of ISO 16.6, features G014 through G020: how many
//! of the paths a pattern matches are kept, per pair of endpoints.
//!
//! There are seven of them and they answer different numbers over the
//! same graph, which is the point of the fixture below: between its two
//! interesting nodes there are paths of three different lengths, and
//! two paths of one of those lengths. A selector read as another one
//! would answer a number these tests do not expect.

use zu::Database;
use zu::zu1::file::Zu1File;
use zu::zu1::graph::bulk_load_as;

const NODES: u32 = 7;

/// Four routes from 0 to 6, of three lengths.
///
/// 0 to 6 direct is one hop. 0 to 1 to 6 and 0 to 2 to 6 are two hops
/// each, so the second length has two paths at it. 0 to 3 to 4 to 5 to 6
/// is four hops. Nothing else reaches 6, so a selector's answer over
/// this graph is a count of paths whose lengths are known.
fn seeded(path: &std::path::Path) {
    let mut db = Zu1File::create(path).expect("create");
    let mut edges: Vec<(u32, u32)> = vec![
        (0, 6),
        (0, 1),
        (1, 6),
        (0, 2),
        (2, 6),
        (0, 3),
        (3, 4),
        (4, 5),
        (5, 6),
    ];
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

    /// The lengths of the paths a selector keeps between 0 and 6, in
    /// hops, sorted. A path of h hops holds 2h+1 elements, which is
    /// what `size` counts, so the halving here is the conversion and not
    /// a fudge.
    fn hops(&mut self, selector: &str) -> Vec<i64> {
        let source = format!(
            "MATCH p = {selector} (a:person {{id: 0}})-[:knows]->+(b:person) \
             WHERE b.id = 6 RETURN size(p) AS n ORDER BY n"
        );
        let rows = self.conn.query(&source).expect("query");
        rows.iter()
            .map(|row| (row.get_by_name::<i64>("n").expect("n") - 1) / 2)
            .collect()
    }
}

/// Every path, which is what a pattern with no selector keeps, so `ALL`
/// and `ALL PATHS` are the same query written more loudly.
#[test]
fn all_keeps_every_path_and_is_the_default() {
    let mut fx = Fixture::open("selector-all.zu1");
    assert_eq!(fx.hops(""), [1, 2, 2, 4]);
    assert_eq!(fx.hops("ALL"), [1, 2, 2, 4]);
    assert_eq!(fx.hops("ALL PATHS"), [1, 2, 2, 4]);
}

/// One path per pair of endpoints, and the standard leaves which one to
/// the engine, so the test says how many rather than which.
#[test]
fn any_keeps_one_path_per_pair_of_endpoints() {
    let mut fx = Fixture::open("selector-any.zu1");
    assert_eq!(fx.hops("ANY").len(), 1);
    assert_eq!(fx.hops("ANY PATHS").len(), 1);
}

/// A count with it keeps that many, and asking for more than there are
/// keeps what there is rather than failing.
#[test]
fn any_k_keeps_at_most_k_paths() {
    let mut fx = Fixture::open("selector-any-k.zu1");
    assert_eq!(fx.hops("ANY 2").len(), 2);
    assert_eq!(fx.hops("ANY 3").len(), 3);
    assert_eq!(fx.hops("ANY 9"), [1, 2, 2, 4]);
}

/// The shortest ones, one of them or all of them. There is a single path
/// of one hop, so the two answer the same number here, which is why the
/// counted forms below matter.
#[test]
fn the_shortest_selectors_keep_the_least_length() {
    let mut fx = Fixture::open("selector-shortest.zu1");
    assert_eq!(fx.hops("ANY SHORTEST"), [1]);
    assert_eq!(fx.hops("ALL SHORTEST"), [1]);
}

/// `SHORTEST k` counts paths. Two of them are the one-hop path and one
/// of the two-hop paths, and three of them take both two-hop paths, so
/// the count reads a length twice.
#[test]
fn shortest_k_counts_paths_and_not_lengths() {
    let mut fx = Fixture::open("selector-shortest-k.zu1");
    assert_eq!(fx.hops("SHORTEST 1"), [1]);
    assert_eq!(fx.hops("SHORTEST 2"), [1, 2]);
    assert_eq!(fx.hops("SHORTEST 3"), [1, 2, 2]);
    assert_eq!(fx.hops("SHORTEST 9"), [1, 2, 2, 4]);
}

/// `SHORTEST k GROUP` counts lengths, and takes every path that has one
/// of them. Two groups is one hop and two hops, which is three paths,
/// where `SHORTEST 2` was two: the group form is the one that cannot
/// answer half of a length.
#[test]
fn shortest_k_group_counts_lengths_and_keeps_every_path_of_them() {
    let mut fx = Fixture::open("selector-shortest-group.zu1");
    assert_eq!(fx.hops("SHORTEST 1 GROUP"), [1]);
    assert_eq!(fx.hops("SHORTEST 2 GROUPS"), [1, 2, 2]);
    assert_eq!(fx.hops("SHORTEST 3 GROUPS"), [1, 2, 2, 4]);
}

/// The standard names two of these twice, and the two spellings of each
/// have to answer the same rows: one shortest path is `ANY SHORTEST` and
/// `SHORTEST 1`, and every shortest path is `ALL SHORTEST` and
/// `SHORTEST 1 GROUP`.
#[test]
fn the_selectors_named_twice_answer_the_same() {
    let mut fx = Fixture::open("selector-pairs.zu1");
    assert_eq!(fx.hops("ANY SHORTEST"), fx.hops("SHORTEST 1"));
    assert_eq!(fx.hops("ALL SHORTEST"), fx.hops("SHORTEST 1 GROUP"));
}

/// A lower bound above one hop used to be refused under a shortest
/// selector, because the search it used reads a hop count as the
/// distance from the start and cannot answer a question about longer
/// paths. It is a fair question: of the paths of at least two hops, the
/// shortest are the two of two hops.
#[test]
fn a_shortest_selector_reads_a_lower_bound() {
    let mut fx = Fixture::open("selector-lower-bound.zu1");
    let source = "MATCH p = ALL SHORTEST (a:person {id: 0})-[:knows]->{2,}(b:person) \
                  WHERE b.id = 6 RETURN size(p) AS n ORDER BY n";
    let rows = fx.conn.query(source).expect("query");
    let hops: Vec<i64> = rows
        .iter()
        .map(|row| (row.get_by_name::<i64>("n").expect("n") - 1) / 2)
        .collect();
    assert_eq!(hops, [2, 2]);
}

/// `KEEP` says the prefix once, behind the patterns, instead of once in
/// front of each of them (ISO 16.9, features G006 and G007). It says the
/// same thing, so it answers the same rows, and it says it to every
/// pattern of the list rather than to the last one written.
#[test]
fn a_keep_says_the_prefix_for_the_whole_pattern_list() {
    let mut fx = Fixture::open("selector-keep.zu1");
    let mut kept = |source: &str| -> Vec<i64> {
        let rows = fx.conn.query(source).expect("query");
        rows.iter()
            .map(|row| (row.get_by_name::<i64>("n").expect("n") - 1) / 2)
            .collect()
    };
    // The same query written both ways answers the same lengths.
    assert_eq!(
        kept(
            "MATCH p = (a:person {id: 0})-[:knows]->+(b:person) KEEP SHORTEST 2 \
             WHERE b.id = 6 RETURN size(p) AS n ORDER BY n"
        ),
        [1, 2]
    );
    // And it reaches every pattern of the list rather than the one it
    // stands behind. Both patterns here run from 0 to 6, which is four
    // paths each and sixteen rows between them, and a KEEP that reached
    // only the second would leave four.
    let mut rows = |source: &str, want: i64| {
        let rows = fx.conn.query(source).expect("query");
        let got: Vec<i64> = rows
            .iter()
            .map(|row| row.get_by_name::<i64>("n").expect("n"))
            .collect();
        assert_eq!(got, [want], "{source}");
    };
    let both = "MATCH (a:person {id: 0})-[:knows]->+(b:person), \
                (a)-[:knows]->+(c:person) WHERE b.id = 6 AND c.id = 6 \
                RETURN COUNT(*) AS n";
    rows(both, 16);
    rows(&both.replace("WHERE", "KEEP ANY SHORTEST WHERE"), 1);
}

/// The plan says which selector is running, because two of them differ
/// only in how many paths come out and a reader of the listing has no
/// other way to tell which search was chosen.
#[test]
fn the_plan_names_the_selector() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("selector-plan.zu1");
    seeded(&path);
    let mut session = zu::session::Session::open(&path).expect("open");
    for (selector, want) in [
        ("ANY", "any 1"),
        ("ANY 3", "any 3"),
        ("ANY SHORTEST", "any shortest"),
        ("ALL SHORTEST", "all shortest"),
        ("SHORTEST 2", "shortest 2"),
        ("SHORTEST 2 GROUPS", "shortest 2 group"),
    ] {
        let plan = session
            .explain(&format!(
                "MATCH {selector} (a:person)-[:knows]->+(b:person) RETURN b.id AS id"
            ))
            .expect("a plan");
        assert!(plan.contains(want), "{selector}: {plan}");
    }
}
