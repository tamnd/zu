//! What arithmetic does where it has no answer, over a stored graph,
//! which is where the pipeline engine's kernels take the plan.
//!
//! ISO 20.21 leaves the arithmetic to the numeric types and the
//! standard's `22012` says what a divisor of nought is: a condition,
//! not a null and not an infinity. The row engine raised it already.
//! What is checked here is that the kernels raise it too, and raise it
//! on the same rows, since a query cannot tell which engine answered
//! it and an answer that depended on that would be no answer at all.

use zu::Database;
use zu::zu1::file::Zu1File;
use zu::zu1::graph::bulk_load_as;

const NODES: u32 = 6;

struct Fixture {
    _dir: tempfile::TempDir,
    conn: zu::Connection,
}

impl Fixture {
    fn open(name: &str) -> Fixture {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(name);
        let mut db = Zu1File::create(&path).expect("create");
        let edges: Vec<(u32, u32)> = (0..NODES).map(|i| (i, (i + 1) % NODES)).collect();
        bulk_load_as(&mut db, "person", "knows", NODES.into(), &edges).expect("load");
        drop(db);
        let db = Database::open(&path).expect("open");
        let conn = db.connect().expect("connect");
        Fixture { _dir: dir, conn }
    }

    /// The condition a statement raised, by its code and its words.
    fn raised(&mut self, source: &str) -> (String, String) {
        let err = self.conn.query(source).expect_err("no answer here");
        let record = err.diagnostic().expect("a condition, not an engine fault");
        (record.status.code().to_string(), record.detail.clone())
    }

    /// The ids a statement answered, in order, for the shapes that have
    /// an answer.
    fn ids(&mut self, source: &str) -> Vec<i64> {
        self.conn
            .query(source)
            .expect("query")
            .iter()
            .map(|row| row.get_by_name::<i64>("id").expect("id"))
            .collect()
    }
}

/// A divisor a row makes nought is the standard's `22012`, and the
/// filter is where it matters: the kernel used to clear the row's
/// validity instead, which reads as a row that failed the predicate, so
/// the statement answered no rows where it should have raised.
#[test]
fn a_divisor_of_nought_in_a_filter_raises() {
    let mut fx = Fixture::open("div-filter.zu1");
    assert_eq!(
        fx.raised("MATCH (p:person) WHERE p.id / (p.id - p.id) > 0 RETURN p.id AS id"),
        ("22012".into(), "division by zero".into())
    );
    assert_eq!(
        fx.raised("MATCH (p:person) WHERE p.id % (p.id - p.id) > 0 RETURN p.id AS id"),
        ("22012".into(), "modulus by zero".into())
    );
}

/// The approximate numbers raise it too. IEEE would answer an infinity
/// and the standard asks for the condition whatever the numeric type,
/// so an engine that hands back the infinity has given a wrong answer
/// rather than raised.
#[test]
fn a_float_divisor_of_nought_raises() {
    let mut fx = Fixture::open("div-float.zu1");
    assert_eq!(
        fx.raised("MATCH (p:person) WHERE 1.0 / (p.id - p.id) > 0 RETURN p.id AS id"),
        ("22012".into(), "division by zero".into())
    );
}

/// A conjunct that decided the row is one the row engine stops at, so
/// the division behind it never runs and the statement answers rather
/// than raising. This is the guard people write, and the query is only
/// right if the engine that takes it reads the conjuncts in the order
/// they were written.
#[test]
fn a_division_behind_a_guard_never_runs() {
    let mut fx = Fixture::open("div-guarded.zu1");
    assert_eq!(
        fx.ids("MATCH (p:person) WHERE p.id > 0 AND 12 / p.id > 5 RETURN p.id AS id"),
        [1, 2]
    );
    // The same statement with the guard taken off, to show what the
    // guard was guarding.
    assert_eq!(
        fx.raised("MATCH (p:person) WHERE 12 / p.id > 5 RETURN p.id AS id"),
        ("22012".into(), "division by zero".into())
    );
}

/// A divisor written as a number that is not nought cannot raise, so
/// the guard above costs that shape nothing and it stays on the kernel.
#[test]
fn a_written_divisor_is_not_a_condition() {
    let mut fx = Fixture::open("div-written.zu1");
    assert_eq!(
        fx.ids("MATCH (p:person) WHERE p.id > 0 AND p.id / 2 > 0 RETURN p.id AS id"),
        [2, 3, 4, 5]
    );
}

/// An answer too wide for an integer is refused rather than wrapped.
/// The row engine adds with a check and raises there, and a kernel that
/// wrapped would answer a negative number for the sum of two positive
/// ones.
#[test]
fn an_integer_answer_that_does_not_fit_is_refused() {
    let mut fx = Fixture::open("overflow.zu1");
    let err = fx
        .conn
        .query("MATCH (p:person) WHERE p.id + 9223372036854775807 > 0 RETURN p.id AS id")
        .expect_err("the sum leaves the integers");
    assert!(
        err.to_string().contains("integer overflow"),
        "{err}, want what went wrong"
    );
}

/// A row the filter already dropped is a row the query never asked
/// about, so a divisor of nought sitting in one is not a condition. The
/// kernels compute every row of a chunk whether it is selected or not,
/// which is what keeps their loops branch free, so this is the
/// difference between a fast kernel and a wrong one.
#[test]
fn a_row_outside_the_selection_raises_nothing() {
    let mut fx = Fixture::open("div-dropped.zu1");
    assert_eq!(
        fx.ids(
            "MATCH (p:person) WHERE p.id > 3 \
             WITH p AS p \
             FILTER 12 / p.id > 1 \
             RETURN p.id AS id"
        ),
        [4, 5]
    );
}
