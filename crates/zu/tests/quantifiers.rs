//! The graph pattern quantifier of ISO 16.10, features G036 and G061:
//! `+`, `*` and `{n,m}` written behind the arrow of a step.
//!
//! It says what zu's `*n..m` inside the brackets says, and the point of
//! these tests is that: the two spellings answer the same rows, so the
//! quantifier is a way of writing a step rather than a second kind of
//! step. It matters because the standard's own examples and the
//! conformance corpus are written this way.

use zu::Database;
use zu::zu1::file::Zu1File;
use zu::zu1::graph::bulk_load_as;

const NODES: u32 = 5;

/// A line with a shortcut: 0 to 1 to 2 to 3 to 4, and 0 straight to 2.
/// Hop counts from 0 are therefore not all distinct, which is what
/// makes a range worth asking about.
fn seeded(path: &std::path::Path) {
    let mut db = Zu1File::create(path).expect("create");
    let mut edges: Vec<(u32, u32)> = vec![(0, 1), (1, 2), (2, 3), (3, 4), (0, 2)];
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

    /// The ids the far end of the step was bound to, sorted, one entry
    /// per path so a step that finds a node twice says so.
    fn reached(&mut self, step: &str) -> Vec<i64> {
        let source =
            format!("MATCH (a:person {{id: 0}}){step}(b:person) RETURN b.id AS id ORDER BY id");
        let rows = self.conn.query(&source).expect("query");
        rows.iter()
            .map(|row| row.get_by_name::<i64>("id").expect("id"))
            .collect()
    }
}

/// Each quantifier against the range it stands for, over a graph where
/// the two would differ if either were read wrongly.
#[test]
fn a_quantifier_answers_what_the_range_answers() {
    let mut fx = Fixture::open("quantifier-parity.zu1");
    for (quantified, ranged) in [
        ("-[:knows]->+", "-[:knows*1..]->"),
        ("-[:knows]->{2}", "-[:knows*2]->"),
        ("-[:knows]->{2,}", "-[:knows*2..]->"),
        ("-[:knows]->{,3}", "-[:knows*..3]->"),
        ("-[:knows]->{1,2}", "-[:knows*1..2]->"),
    ] {
        let one = fx.reached(quantified);
        let other = fx.reached(ranged);
        assert_eq!(one, other, "{quantified} against {ranged}");
        assert!(!one.is_empty(), "{quantified} answered nothing");
    }
}

/// What the counts are, so that the parity above is parity with
/// something right rather than with the same mistake twice. Two hops
/// from 0 reaches 2 by the shortcut and 3 the long way, and the trail
/// mode leaves nothing else.
#[test]
fn a_quantified_step_counts_the_paths_and_not_the_nodes() {
    let mut fx = Fixture::open("quantifier-counts.zu1");
    assert_eq!(fx.reached("-[:knows]->{2}"), [2, 3]);
    assert_eq!(fx.reached("-[:knows]->{1,2}"), [1, 2, 2, 3]);
}

/// `*` is zero or more, and a step of no hops is a thing zu does not
/// walk yet, so it is refused with the reason rather than answered as
/// though it had been written `+`. A quantifier whose lower bound is
/// written 0 gets the same answer, because it is the same request.
#[test]
fn a_step_of_no_hops_is_refused_and_not_rounded_up() {
    let mut fx = Fixture::open("quantifier-zero.zu1");
    for step in ["-[:knows]->*", "-[:knows]->{0,2}"] {
        let source = format!("MATCH (a:person {{id: 0}}){step}(b:person) RETURN b.id AS id");
        let err = fx
            .conn
            .query(&source)
            .expect_err("this one does not run")
            .to_string();
        assert!(err.contains("zero-length hops"), "{step}: {err}");
    }
}

/// The abbreviated edge pattern takes a quantifier too, which is the
/// shortest way the standard writes a repeated step.
#[test]
fn the_abbreviated_edge_pattern_takes_one() {
    let mut fx = Fixture::open("quantifier-abbreviated.zu1");
    assert_eq!(fx.reached("-->{2}"), fx.reached("-[:knows]->{2}"));
}
