//! `FOR`, the statement that makes a row out of every element of a
//! list (ISO 14.8, features GQ10, GQ11 and GQ24).
//!
//! It is the one way a linear statement makes rows out of a value
//! rather than out of the graph, so what is checked here is that: the
//! rows it makes, the counter it may number them with, and what
//! happens when it stands under a match and runs once per row.

use zu::Database;
use zu::zu1::file::Zu1File;
use zu::zu1::graph::bulk_load_as;

const NODES: u32 = 6;

fn seeded(path: &std::path::Path) {
    let mut db = Zu1File::create(path).expect("create");
    let mut edges: Vec<(u32, u32)> = (0..NODES).map(|i| (i, (i + 1) % NODES)).collect();
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

    /// The `v` column of every row, sorted, since a list has an order
    /// but a result table does not promise one.
    fn values(&mut self, source: &str) -> Vec<i64> {
        let rows = self.conn.query(source).expect("query");
        let mut values: Vec<i64> = rows
            .iter()
            .map(|row| row.get_by_name::<i64>("v").expect("an integer column"))
            .collect();
        values.sort_unstable();
        values
    }

    /// The `v` and `n` columns together, sorted on the value, which is
    /// what a counter has to be read against.
    fn pairs(&mut self, source: &str) -> Vec<(i64, i64)> {
        let rows = self.conn.query(source).expect("query");
        let mut pairs: Vec<(i64, i64)> = rows
            .iter()
            .map(|row| {
                (
                    row.get_by_name::<i64>("v").expect("v"),
                    row.get_by_name::<i64>("n").expect("n"),
                )
            })
            .collect();
        pairs.sort_unstable();
        pairs
    }
}

/// The statement itself: one row per element, and the same rows the
/// Cypher spelling of it answers.
#[test]
fn for_makes_a_row_of_every_element() {
    let mut fx = Fixture::open("for.zu1");
    assert_eq!(fx.values("FOR x IN [1, 2, 3] RETURN x AS v"), [1, 2, 3]);
    let unwound = fx.values("UNWIND [1, 2, 3] AS x RETURN x AS v");
    assert_eq!(unwound, [1, 2, 3], "the two spellings are one statement");
    assert_eq!(
        fx.values("FOR x IN [] RETURN x AS v"),
        Vec::<i64>::new(),
        "an empty list makes no rows, so nothing downstream runs"
    );
}

/// `WITH ORDINALITY` counts from one and `WITH OFFSET` from zero, and
/// that is the whole of the difference between them.
#[test]
fn a_counter_numbers_the_elements() {
    let mut fx = Fixture::open("for-counter.zu1");
    assert_eq!(
        fx.pairs("FOR x IN [10, 20] WITH ORDINALITY i RETURN x AS v, i AS n"),
        [(10, 1), (20, 2)]
    );
    assert_eq!(
        fx.pairs("FOR x IN [10, 20] WITH OFFSET i RETURN x AS v, i AS n"),
        [(10, 0), (20, 1)]
    );
}

/// The counter numbers the elements of a list, so a `FOR` that runs
/// once per row starts again at each of them rather than counting the
/// rows it has answered altogether.
#[test]
fn the_counter_starts_again_at_each_row() {
    let mut fx = Fixture::open("for-per-row.zu1");
    let pairs = fx.pairs(
        "MATCH (p:person) WHERE p.id < 3 \
         FOR x IN [7, 8] WITH ORDINALITY i \
         RETURN p.id * 100 + x AS v, i AS n",
    );
    assert_eq!(
        pairs,
        [(7, 1), (8, 2), (107, 1), (108, 2), (207, 1), (208, 2)],
        "three rows in, two elements each, numbered one and two every time"
    );
}

/// A `WITH` after a `FOR` is still a projection unless the word after
/// it says otherwise, which is the one place the two readings of the
/// keyword meet.
#[test]
fn with_after_for_is_still_a_projection() {
    let mut fx = Fixture::open("for-with.zu1");
    assert_eq!(
        fx.values("FOR x IN [1, 2, 3] WITH x AS y WHERE y > 1 RETURN y AS v"),
        [2, 3]
    );
}

/// The value the statement walks has to be a list, and something that
/// is not one is refused by name rather than answering no rows.
#[test]
fn for_needs_a_list() {
    let mut fx = Fixture::open("for-scalar.zu1");
    let err = fx
        .conn
        .query("FOR x IN 3 RETURN x AS v")
        .expect_err("an integer is not a list");
    assert!(
        err.to_string().contains("needs a list"),
        "{err}, want the reason"
    );
}

/// The counter is a variable like the element is, so naming it twice,
/// or naming it what the element is called, is the redefinition it
/// looks like.
#[test]
fn the_counter_is_a_name_of_its_own() {
    let mut fx = Fixture::open("for-shadow.zu1");
    let err = fx
        .conn
        .query("FOR x IN [1, 2] WITH ORDINALITY x RETURN x AS v")
        .expect_err("x is the element");
    assert!(
        err.to_string().contains("'x' is already defined"),
        "{err}, want the name that was taken"
    );
}

/// A `FOR` reads what the statement already has, so the list may be
/// something an earlier clause worked out rather than a literal.
#[test]
fn for_walks_a_list_the_statement_made() {
    let mut fx = Fixture::open("for-computed.zu1");
    let values = fx.values(
        "MATCH (p:person) WHERE p.id = 2 \
         LET xs = [p.id, p.id * 2] \
         FOR x IN xs \
         RETURN x AS v",
    );
    assert_eq!(values, [2, 4]);
}
