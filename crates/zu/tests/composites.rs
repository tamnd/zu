//! The composite query statement: `UNION`, `EXCEPT`, `INTERSECT` and
//! `OTHERWISE` over two result tables (ISO 12.1, features GQ02 to
//! GQ07).
//!
//! The unit tests in `zu_query` check that a composite parses into
//! operands and plans into a conjoin with a build side on it. What is
//! checked here is what it answers, over a real store, for all six
//! spellings of the set operators and for the one conjunction that is
//! not a set operator at all.

use zu::Database;
use zu::zu1::file::Zu1File;
use zu::zu1::graph::bulk_load_as;

const NODES: u32 = 20;

/// A ring of twenty people, each pointing at the next and at the one
/// seven along, which gives every id an answer and no id the same
/// answer twice.
fn seeded(path: &std::path::Path) {
    let mut db = Zu1File::create(path).expect("create");
    let mut edges: Vec<(u32, u32)> = (0..NODES)
        .flat_map(|i| [(i, (i + 1) % NODES), (i, (i + 7) % NODES)])
        .collect();
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

    /// The `id` column of a query, sorted, so a test says what rows
    /// came back and not what order the storage handed them over in.
    /// The standard leaves that order to the running system.
    fn ids(&mut self, source: &str) -> Vec<i64> {
        let rows = self.conn.query(source).expect("query");
        let mut ids: Vec<i64> = rows
            .iter()
            .map(|row| row.get_by_name::<i64>("id").expect("an integer column"))
            .collect();
        ids.sort_unstable();
        ids
    }
}

/// A plain `UNION` is `UNION DISTINCT`, which is the standard's default
/// set quantifier and the thing that catches an implementation that
/// read it as a concatenation.
#[test]
fn union_without_a_quantifier_answers_each_row_once() {
    let mut fx = Fixture::open("union.zu1");
    let both = fx.ids(
        "MATCH (p:person) WHERE p.id < 4 RETURN p.id AS id \
         UNION \
         MATCH (q:person) WHERE q.id > 1 AND q.id < 6 RETURN q.id AS id",
    );
    assert_eq!(both, [0, 1, 2, 3, 4, 5]);
}

/// `UNION ALL` keeps what `UNION` removes, which is the whole of the
/// difference between the two.
#[test]
fn union_all_keeps_the_rows_union_removes() {
    let mut fx = Fixture::open("union-all.zu1");
    let both = fx.ids(
        "MATCH (p:person) WHERE p.id < 4 RETURN p.id AS id \
         UNION ALL \
         MATCH (q:person) WHERE q.id > 1 AND q.id < 6 RETURN q.id AS id",
    );
    assert_eq!(both, [0, 1, 2, 2, 3, 3, 4, 5]);
}

/// `EXCEPT` subtracts, and its two quantifiers differ only in how many
/// copies one occurrence on the right takes away.
#[test]
fn except_subtracts_the_right_operand() {
    let mut fx = Fixture::open("except.zu1");
    let distinct = fx.ids(
        "MATCH (p:person) WHERE p.id < 5 RETURN p.id AS id \
         EXCEPT DISTINCT \
         MATCH (q:person) WHERE q.id < 2 RETURN q.id AS id",
    );
    assert_eq!(distinct, [2, 3, 4]);

    // The left answers each id twice, once down each of the two edges
    // out of it, and the right answers it once, so ALL leaves one of
    // the two standing where DISTINCT leaves none.
    let all = fx.ids(
        "MATCH (p:person)-[:knows]->() WHERE p.id < 3 RETURN p.id AS id \
         EXCEPT ALL \
         MATCH (q:person) WHERE q.id < 3 RETURN q.id AS id",
    );
    assert_eq!(all, [0, 1, 2]);
    let none = fx.ids(
        "MATCH (p:person)-[:knows]->() WHERE p.id < 3 RETURN p.id AS id \
         EXCEPT DISTINCT \
         MATCH (q:person) WHERE q.id < 3 RETURN q.id AS id",
    );
    assert!(none.is_empty(), "{none:?}, want nothing left");
}

/// `INTERSECT` keeps what both answered, one occurrence per pair of
/// occurrences under `ALL` and one per value under `DISTINCT`.
#[test]
fn intersect_keeps_what_both_operands_answered() {
    let mut fx = Fixture::open("intersect.zu1");
    let distinct = fx.ids(
        "MATCH (p:person) WHERE p.id < 5 RETURN p.id AS id \
         INTERSECT DISTINCT \
         MATCH (q:person) WHERE q.id > 2 RETURN q.id AS id",
    );
    assert_eq!(distinct, [3, 4]);

    let all = fx.ids(
        "MATCH (p:person)-[:knows]->() WHERE p.id < 2 RETURN p.id AS id \
         INTERSECT ALL \
         MATCH (q:person) WHERE q.id < 2 RETURN q.id AS id",
    );
    assert_eq!(all, [0, 1], "one pair each, not two");
}

/// `OTHERWISE` is not a set operator. It is a choice between two
/// answers, and the right one is the answer only when the left had
/// none.
#[test]
fn otherwise_answers_the_right_operand_only_when_the_left_is_empty() {
    let mut fx = Fixture::open("otherwise.zu1");
    let fallback = fx.ids(
        "MATCH (p:person) WHERE p.id > 1000 RETURN p.id AS id \
         OTHERWISE \
         MATCH (q:person) WHERE q.id < 2 RETURN q.id AS id",
    );
    assert_eq!(fallback, [0, 1], "the left found nothing, so the right ran");

    let first = fx.ids(
        "MATCH (p:person) WHERE p.id < 2 RETURN p.id AS id \
         OTHERWISE \
         MATCH (q:person) WHERE q.id < 5 RETURN q.id AS id",
    );
    assert_eq!(first, [0, 1], "the left answered, so the right did not run");
}

/// The operators are left associative and all at one level, so three
/// operands read left to right and not by any precedence between the
/// words.
#[test]
fn three_operands_read_left_to_right() {
    let mut fx = Fixture::open("three.zu1");
    // (0,1,2,3 UNION 4) EXCEPT 0,1 leaves 2, 3, 4. Read the other way
    // round, as 0..3 UNION (4 EXCEPT 0,1), it would leave 0 to 4.
    let ids = fx.ids(
        "MATCH (p:person) WHERE p.id < 4 RETURN p.id AS id \
         UNION \
         MATCH (q:person) WHERE q.id = 4 RETURN q.id AS id \
         EXCEPT \
         MATCH (r:person) WHERE r.id < 2 RETURN r.id AS id",
    );
    assert_eq!(ids, [2, 3, 4]);
}

/// The operands meet column by column, so a pair that does not agree
/// on its columns is refused rather than answered with whichever
/// shape came first.
#[test]
fn operands_have_to_agree_on_their_columns() {
    let mut fx = Fixture::open("columns.zu1");
    let err = fx
        .conn
        .query(
            "MATCH (p:person) RETURN p.id AS id \
             UNION \
             MATCH (q:person) RETURN q.id AS other",
        )
        .expect_err("the columns disagree");
    assert!(
        err.to_string().contains("column by column"),
        "{err}, want the mismatched column names"
    );

    let err = fx
        .conn
        .query(
            "MATCH (p:person) RETURN p.id AS id \
             UNION \
             MATCH (q:person) RETURN q.id AS id, q.id AS again",
        )
        .expect_err("the degrees disagree");
    assert!(
        err.to_string().contains("1 and 2 columns"),
        "{err}, want the column counts"
    );
}

/// A variable is an operand's own. The composite meets two tables of
/// values, so a name one operand bound is no more visible to the other
/// than a name in another query would be.
#[test]
fn an_operand_cannot_read_what_the_other_bound() {
    let mut fx = Fixture::open("scope.zu1");
    let err = fx
        .conn
        .query(
            "MATCH (p:person) RETURN p.id AS id \
             UNION \
             MATCH (q:person) WHERE q.id = p.id RETURN q.id AS id",
        )
        .expect_err("p belongs to the left operand");
    assert!(
        err.to_string().contains("'p' is not defined"),
        "{err}, want the undefined variable"
    );
}
