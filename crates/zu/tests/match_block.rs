//! Blocks of several match statements (ISO 14.4 and 19.4, features
//! GQ21 and GQ22).
//!
//! A block is one conjunction: the statements in it are all required
//! and they share the names they write. What that buys is a two hop
//! reach written as two statements rather than as one pattern, either
//! asked about with `EXISTS` or kept with `OPTIONAL`, and the second of
//! those is where the interesting rule is, since a block that finds
//! nothing nulls every name it writes rather than dropping the row.

use zu::Database;
use zu::zu1::file::Zu1File;
use zu::zu1::graph::bulk_load_as;

const NODES: u32 = 5;

/// A path, 0 to 1 to 2 to 3, and a 4 that nothing touches. Two hops
/// leave 0 and 1, one hop leaves 2, and nothing leaves 3 or 4, so every
/// answer below is a different subset of the same five nodes.
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
}

/// The statements in a block are all required, so a block of two is the
/// two hop reach and not either hop on its own.
#[test]
fn exists_over_two_statements_asks_for_both() {
    let mut fx = Fixture::open("exists-block.zu1");
    let two_hops = fx.ids(
        "MATCH (p:person) \
         WHERE EXISTS { MATCH (p)-[:knows]->(q:person) MATCH (q)-[:knows]->(r:person) } \
         RETURN p.id AS id",
    );
    assert_eq!(two_hops, [0, 1]);
    let one_hop = fx.ids(
        "MATCH (p:person) \
         WHERE EXISTS { MATCH (p)-[:knows]->(q:person) } \
         RETURN p.id AS id",
    );
    assert_eq!(one_hop, [0, 1, 2], "one statement is the one hop reach");
}

/// Writing the statements out is writing the conjunction the commas
/// already meant, so the two spellings answer the same rows.
#[test]
fn a_block_is_the_conjunction_the_commas_are() {
    let mut fx = Fixture::open("exists-comma.zu1");
    let written_out = fx.ids(
        "MATCH (p:person) \
         WHERE EXISTS { MATCH (p)-[:knows]->(q:person) MATCH (q)-[:knows]->(r:person) } \
         RETURN p.id AS id",
    );
    let commas = fx.ids(
        "MATCH (p:person) \
         WHERE EXISTS { MATCH (p)-[:knows]->(q:person), (q)-[:knows]->(r:person) } \
         RETURN p.id AS id",
    );
    assert_eq!(written_out, commas);
}

/// Each statement may carry a condition of its own, and since nothing
/// in a block is optional the conditions are one condition.
#[test]
fn every_statement_may_carry_its_own_where() {
    let mut fx = Fixture::open("exists-where.zu1");
    let ids = fx.ids(
        "MATCH (p:person) \
         WHERE EXISTS { MATCH (p)-[:knows]->(q:person) WHERE q.id > 1 \
                        MATCH (q)-[:knows]->(r:person) WHERE r.id > 2 } \
         RETURN p.id AS id",
    );
    assert_eq!(ids, [1], "only 1 reaches a 2 that reaches a 3");
}

/// An OPTIONAL block keeps what it matched, so the names it writes are
/// readable after it.
#[test]
fn an_optional_block_keeps_what_it_matched() {
    let mut fx = Fixture::open("optional-block.zu1");
    let rows = fx
        .conn
        .query(
            "MATCH (p:person) \
             WHERE p.id = 0 \
             OPTIONAL { MATCH (p)-[:knows]->(q:person) MATCH (q)-[:knows]->(r:person) } \
             RETURN r.id AS id",
        )
        .expect("query");
    let reach: Vec<i64> = rows
        .iter()
        .map(|row| row.get_by_name::<i64>("id").expect("id"))
        .collect();
    assert_eq!(reach, [2], "0 reaches 2 in two hops");
}

/// The row survives a block that finds nothing, which is the whole
/// difference between an OPTIONAL block and a required one.
#[test]
fn a_block_that_finds_nothing_nulls_its_names() {
    let mut fx = Fixture::open("optional-null.zu1");
    let rows = fx
        .conn
        .query(
            "MATCH (p:person) \
             WHERE p.id = 4 \
             OPTIONAL { MATCH (p)-[:knows]->(q:person) MATCH (q)-[:knows]->(r:person) } \
             RETURN p.id AS id, r.id AS reach",
        )
        .expect("query");
    assert_eq!(rows.rows.len(), 1, "the row is kept");
    let row = rows.iter().next().expect("one row");
    assert_eq!(row.get_by_name::<i64>("id").expect("id"), 4);
    assert!(
        row.get_by_name::<i64>("reach").is_err(),
        "nothing was reached, so the column is null"
    );
}

/// A block is one operand rather than one operand per statement, so a
/// half match is no match: 2 has a first hop and no second, and the
/// name the first hop wrote is null along with the name the second one
/// would have written.
#[test]
fn half_a_block_is_none_of_it() {
    let mut fx = Fixture::open("optional-half.zu1");
    let rows = fx
        .conn
        .query(
            "MATCH (p:person) \
             WHERE p.id = 2 \
             OPTIONAL { MATCH (p)-[:knows]->(q:person) MATCH (q)-[:knows]->(r:person) } \
             RETURN q.id AS first, r.id AS later",
        )
        .expect("query");
    assert_eq!(rows.rows.len(), 1);
    let row = rows.iter().next().expect("one row");
    assert!(
        row.get_by_name::<i64>("first").is_err(),
        "the hop is undone"
    );
    assert!(row.get_by_name::<i64>("later").is_err());
}
