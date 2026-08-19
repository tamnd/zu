//! The existence predicate in the shapes ISO 19.4 writes it in.
//!
//! There are three: a graph pattern, a block of match statements, and
//! a whole query. The first two are the same thing to the parser, a
//! block being a pattern with more matches behind it, and either may
//! stand in braces or in parentheses. The third is different in kind,
//! since a query has clauses of its own and ends with a RETURN, and it
//! is the one this file is mostly about. What is asked of it is only
//! whether it answered a row, so what it returns is never read and the
//! run stops at the first row it makes.

use zu::Database;
use zu::zu1::file::Zu1File;
use zu::zu1::graph::bulk_load_as;

const NODES: u32 = 5;

/// A path, 0 to 1 to 2 to 3, and a 4 that nothing touches.
fn seeded(path: &std::path::Path) {
    let mut db = Zu1File::create(path).expect("create");
    let edges: Vec<(u32, u32)> = vec![(0, 1), (1, 2), (2, 3)];
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

    /// The `id` column, sorted, since none of these ask for an order.
    fn ids(&mut self, source: &str) -> Vec<i64> {
        let rows = self.conn.query(source).expect("query");
        let mut ids: Vec<i64> = rows
            .iter()
            .map(|row| row.get_by_name::<i64>("id").expect("id"))
            .collect();
        ids.sort_unstable();
        ids
    }

    fn err(&mut self, source: &str) -> String {
        self.conn.query(source).expect_err("refused").to_string()
    }
}

/// Parentheses hold what braces hold. The standard writes both and the
/// pattern inside is the same pattern.
#[test]
fn a_pattern_may_stand_in_parentheses() {
    let mut fx = Fixture::open("exists-parens.zu1");
    let braced = fx.ids(
        "MATCH (p:person) \
         WHERE EXISTS { (p)-[:knows]->(q:person) } \
         RETURN p.id AS id",
    );
    let parenthesized = fx.ids(
        "MATCH (p:person) \
         WHERE EXISTS ( (p)-[:knows]->(q:person) ) \
         RETURN p.id AS id",
    );
    assert_eq!(braced, [0, 1, 2]);
    assert_eq!(parenthesized, braced);
}

/// A block of several statements may stand in parentheses too, since
/// what the brackets hold is the block and not one statement of it.
#[test]
fn a_block_may_stand_in_parentheses() {
    let mut fx = Fixture::open("exists-block-parens.zu1");
    let ids = fx.ids(
        "MATCH (p:person) \
         WHERE EXISTS ( MATCH (p)-[:knows]->(q:person) MATCH (q)-[:knows]->(r:person) ) \
         RETURN p.id AS id",
    );
    assert_eq!(ids, [0, 1], "two hops leave 0 and 1");
}

/// A whole query inside, which is the third shape. The RETURN is what
/// tells it from a block, and the rows it returns are counted rather
/// than read.
#[test]
fn a_query_inside_is_asked_only_whether_it_answered() {
    let mut fx = Fixture::open("exists-query.zu1");
    let ids = fx.ids(
        "MATCH (p:person) \
         WHERE EXISTS { MATCH (q:person)-[:knows]->(r:person) WHERE q.id = p.id RETURN r.id } \
         RETURN p.id AS id",
    );
    assert_eq!(ids, [0, 1, 2]);
}

/// Nothing asks what the query returns, so it may return anything at
/// all: several columns, a constant, or a value no column holds. A
/// value query would have refused all three.
#[test]
fn what_the_query_returns_is_never_read() {
    let mut fx = Fixture::open("exists-columns.zu1");
    let two_columns = fx.ids(
        "MATCH (p:person) \
         WHERE EXISTS { MATCH (q:person)-[:knows]->(r:person) WHERE q.id = p.id \
                        RETURN r.id AS a, q.id AS b } \
         RETURN p.id AS id",
    );
    assert_eq!(two_columns, [0, 1, 2]);
    let constant = fx.ids(
        "MATCH (p:person) \
         WHERE EXISTS { MATCH (q:person)-[:knows]->(r:person) WHERE q.id = p.id \
                        RETURN 1 AS one } \
         RETURN p.id AS id",
    );
    assert_eq!(constant, two_columns);
}

/// A query that answers no row is false rather than null, which is the
/// difference between asking whether something is there and asking
/// what it is.
#[test]
fn a_query_that_answers_nothing_is_false() {
    let mut fx = Fixture::open("exists-empty.zu1");
    let ids = fx.ids(
        "MATCH (p:person) \
         WHERE NOT EXISTS { MATCH (q:person)-[:knows]->(r:person) WHERE q.id = p.id \
                            RETURN r.id } \
         RETURN p.id AS id",
    );
    assert_eq!(ids, [3, 4], "nothing leaves 3 or 4");
}

/// The query reads the row it is written inside, which is what makes
/// the answer that row's rather than the statement's.
#[test]
fn the_query_reads_the_row_around_it() {
    let mut fx = Fixture::open("exists-correlated.zu1");
    let ids = fx.ids(
        "MATCH (p:person) \
         WHERE EXISTS { MATCH (q:person) WHERE q.id = p.id + 1 RETURN q.id } \
         RETURN p.id AS id",
    );
    assert_eq!(ids, [0, 1, 2, 3], "everyone but the last has a successor");
}

/// One that reads nothing is the same answer for every row, and the
/// answer is the same whichever way it goes.
#[test]
fn one_that_reads_nothing_answers_every_row_alike() {
    let mut fx = Fixture::open("exists-constant.zu1");
    let all = fx.ids(
        "MATCH (p:person) \
         WHERE EXISTS { MATCH (q:person) RETURN q.id } \
         RETURN p.id AS id",
    );
    assert_eq!(all, [0, 1, 2, 3, 4]);
    let none = fx.ids(
        "MATCH (p:person) \
         WHERE EXISTS { MATCH (q:person) WHERE q.id > 99 RETURN q.id } \
         RETURN p.id AS id",
    );
    assert!(none.is_empty());
}

/// It stands where a value stands, not only under a WHERE, since it is
/// an expression and answers a boolean.
#[test]
fn it_stands_where_a_boolean_stands() {
    let mut fx = Fixture::open("exists-projected.zu1");
    let rows = fx
        .conn
        .query(
            "MATCH (p:person) \
             WHERE p.id = 3 \
             RETURN EXISTS { MATCH (q:person)-[:knows]->(r:person) WHERE q.id = p.id \
                             RETURN r.id } AS onward",
        )
        .expect("query");
    let row = rows.iter().next().expect("one row");
    assert!(!row.get_by_name::<bool>("onward").expect("onward"));
}

/// A query inside has a scope of its own, so a pattern in it that
/// writes a name the row already carries would be two elements under
/// one word rather than the one the reader meant. That is refused, and
/// the message points at the block form, which is where a pattern and
/// the row around it are the same element.
#[test]
fn a_pattern_inside_may_not_write_a_name_the_row_carries() {
    let mut fx = Fixture::open("exists-shadow.zu1");
    let err = fx.err(
        "MATCH (p:person) \
         WHERE EXISTS { MATCH (p)-[:knows]->(q:person) RETURN q.id } \
         RETURN p.id AS id",
    );
    assert!(
        err.contains("'p' is a name the query around this EXISTS already wrote"),
        "{err}"
    );
    assert!(err.contains("block form"), "{err}");
    let block = fx.ids(
        "MATCH (p:person) \
         WHERE EXISTS { MATCH (p)-[:knows]->(q:person) } \
         RETURN p.id AS id",
    );
    assert_eq!(block, [0, 1, 2], "which is what the block form answers");
}

/// A query written here is read where a value belongs, so it may not
/// change the graph, and a USE of its own would be a second graph in a
/// statement that runs in one.
#[test]
fn a_query_inside_may_not_write_or_carry_a_use() {
    let mut fx = Fixture::open("exists-refusals.zu1");
    assert!(
        fx.err(
            "MATCH (p:person) \
             WHERE EXISTS { INSERT (:person {id: 9}) RETURN 1 AS one } \
             RETURN p.id AS id"
        )
        .contains("may not write to the graph")
    );
    assert!(
        fx.err(
            "MATCH (p:person) \
             WHERE EXISTS { USE other MATCH (q:person) RETURN q.id } \
             RETURN p.id AS id"
        )
        .contains("may not carry a USE of its own")
    );
}
