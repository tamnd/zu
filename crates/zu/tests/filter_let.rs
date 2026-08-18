//! `FILTER` and `LET`, the two statements GQL has where Cypher has a
//! clause of a projection (ISO 14.6 and 14.7, features GQ08 and GQ09).
//!
//! Both are about what a statement carries rather than what it finds,
//! so what is checked here is the carrying: that a FILTER reads the
//! rows the statement already has, that a LET adds a name without
//! taking any away, and that the two compose with the statement
//! chaining the milestone before them built.

use zu::Database;
use zu::zu1::file::Zu1File;
use zu::zu1::graph::bulk_load_as;

const NODES: u32 = 20;

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

/// A FILTER is a WHERE with no pattern under it, so it answers what
/// the same condition written on the MATCH answers.
#[test]
fn filter_keeps_the_rows_the_condition_holds_for() {
    let mut fx = Fixture::open("filter.zu1");
    let filtered = fx.ids("MATCH (p:person) FILTER p.id < 4 RETURN p.id AS id");
    assert_eq!(filtered, [0, 1, 2, 3]);
    let wheres = fx.ids("MATCH (p:person) WHERE p.id < 4 RETURN p.id AS id");
    assert_eq!(filtered, wheres);
    // The standard's optional WHERE says nothing extra.
    let spelled = fx.ids("MATCH (p:person) FILTER WHERE p.id < 4 RETURN p.id AS id");
    assert_eq!(filtered, spelled);
}

/// The point of the statement is that it stands where no pattern was
/// written: over the rows a chain handed on, and over a second
/// condition on rows a first FILTER already cut down.
#[test]
fn filter_reads_what_the_statement_already_has() {
    let mut fx = Fixture::open("filter-chain.zu1");
    let chained = fx.ids(
        "MATCH (p:person)-[:knows]->(f) RETURN f.id AS id \
         NEXT FILTER id < 4 RETURN id AS id",
    );
    assert_eq!(
        chained,
        [0, 0, 1, 1, 2, 2, 3, 3],
        "each id is the end of two edges, one along and one seven along"
    );
    let twice = fx.ids("MATCH (p:person) FILTER p.id < 8 FILTER p.id > 4 RETURN p.id AS id");
    assert_eq!(twice, [5, 6, 7]);
}

/// A LET adds a name and takes none away, which is the whole of the
/// difference between it and a WITH.
#[test]
fn let_adds_a_name_without_dropping_the_others() {
    let mut fx = Fixture::open("let.zu1");
    let rows = fx
        .conn
        .query("MATCH (p:person) WHERE p.id = 3 LET twice = p.id * 2 RETURN p.id AS id, twice AS t")
        .expect("query");
    let row = rows.iter().next().expect("one row");
    assert_eq!(row.get_by_name::<i64>("id").expect("id"), 3);
    assert_eq!(row.get_by_name::<i64>("t").expect("t"), 6);
    assert_eq!(rows.rows.len(), 1);
}

/// The definitions read left to right, so a later one may use a name
/// an earlier one in the same statement gave.
#[test]
fn a_later_definition_reads_an_earlier_one() {
    let mut fx = Fixture::open("let-chain.zu1");
    let rows = fx
        .conn
        .query(
            "MATCH (p:person) WHERE p.id = 3 \
             LET twice = p.id * 2, more = twice + 1 \
             RETURN more AS id",
        )
        .expect("query");
    let row = rows.iter().next().expect("one row");
    assert_eq!(row.get_by_name::<i64>("id").expect("id"), 7);
    assert_eq!(rows.rows.len(), 1);
}

/// The two together are the shape they are for: name something, then
/// keep the rows it holds for.
#[test]
fn let_and_filter_compose() {
    let mut fx = Fixture::open("let-filter.zu1");
    let ids = fx.ids(
        "MATCH (p:person)-[:knows]->(f) \
         LET gap = f.id - p.id \
         FILTER gap = 7 \
         RETURN p.id AS id",
    );
    assert_eq!(ids, (0..13).collect::<Vec<i64>>(), "the seven-along edge");
}

/// A LET names a variable, and a name already in scope is already a
/// name, so redefining it is refused rather than quietly meaning the
/// second one from there on.
#[test]
fn a_let_may_not_redefine_a_name_in_scope() {
    let mut fx = Fixture::open("let-shadow.zu1");
    let err = fx
        .conn
        .query("MATCH (p:person) LET p = 1 RETURN p AS id")
        .expect_err("p is the matched node");
    assert!(
        err.to_string().contains("'p' is already defined"),
        "{err}, want the name that was taken"
    );
}

/// A set function reads a group of rows and a LET reads one, so an
/// aggregate here is refused with the clause that does group rows
/// named rather than with a type error out of the evaluator.
#[test]
fn a_let_cannot_name_an_aggregate() {
    let mut fx = Fixture::open("let-agg.zu1");
    let err = fx
        .conn
        .query("MATCH (p:person) LET n = count(*) RETURN n AS id")
        .expect_err("count is over a group");
    assert!(
        err.to_string().contains("cannot be an aggregate"),
        "{err}, want the reason"
    );
}

/// `LET p.age = 30` is a write written where a definition goes. The
/// message says which statement does that rather than reporting a
/// syntax error at the dot.
#[test]
fn let_of_a_property_says_to_use_set() {
    let mut fx = Fixture::open("let-prop.zu1");
    let err = fx
        .conn
        .query("MATCH (p:person) LET p.id = 1 RETURN p.id AS id")
        .expect_err("a property is not a variable");
    assert!(
        err.to_string()
            .contains("changing a property of an element is SET"),
        "{err}, want the statement that does it"
    );
}
