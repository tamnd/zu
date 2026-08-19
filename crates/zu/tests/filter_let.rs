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

/// GE03. The same word in a smaller place: a name for something worked
/// out once, for the length of one expression and gone at the END.
#[test]
fn a_let_written_inside_an_expression_names_a_value() {
    let mut fx = Fixture::open("let-expr.zu1");
    let rows = fx
        .conn
        .query("RETURN LET n = 2 + 3 IN n * n END AS id")
        .expect("query");
    let row = rows.iter().next().expect("one row");
    assert_eq!(row.get_by_name::<i64>("id").expect("id"), 25);
}

/// The definitions read left to right here the way they do in the
/// clause, so a pair where the second is about the first reads the way
/// it is written.
#[test]
fn a_later_definition_inside_an_expression_reads_an_earlier_one() {
    let mut fx = Fixture::open("let-expr-chain.zu1");
    let rows = fx
        .conn
        .query("RETURN LET a = 4, b = a + 1 IN a * b END AS id")
        .expect("query");
    let row = rows.iter().next().expect("one row");
    assert_eq!(row.get_by_name::<i64>("id").expect("id"), 20);
}

/// The name is a name and everything that works on one works on it, so
/// what a definition holds may be a node the match found and the body
/// may read a property off it.
#[test]
fn a_definition_may_hold_what_the_match_found() {
    let mut fx = Fixture::open("let-expr-node.zu1");
    let ids = fx.ids(
        "MATCH (p:person) \
         RETURN LET twice = p.id * 2 IN twice - p.id END AS id",
    );
    assert_eq!(ids, (0..i64::from(NODES)).collect::<Vec<i64>>());
}

/// The names are gone at the END, which is the whole difference between
/// this and the clause: what the clause adds to the row, this adds to
/// nothing.
#[test]
fn a_name_written_inside_an_expression_is_gone_after_it() {
    let mut fx = Fixture::open("let-expr-scope.zu1");
    let err = fx
        .conn
        .query("RETURN LET n = 1 IN n END AS a, n AS b")
        .expect_err("n was the other item's name");
    assert!(
        err.to_string().contains("'n'"),
        "{err}, want the name that is not there"
    );
}

/// A name in scope is refused rather than shadowed, which is the rule
/// the clause already keeps.
#[test]
fn a_definition_inside_an_expression_may_not_take_a_name_in_scope() {
    let mut fx = Fixture::open("let-expr-shadow.zu1");
    let err = fx
        .conn
        .query("MATCH (p:person) RETURN LET p = 1 IN p END AS id")
        .expect_err("p is the matched node");
    assert!(
        err.to_string().contains("'p' is already defined"),
        "{err}, want the name that was taken"
    );
}

/// An item and a name written inside it may be spelled the same, since
/// the name is gone before the item has one.
#[test]
fn an_item_may_be_named_after_what_was_written_inside_it() {
    let mut fx = Fixture::open("let-expr-alias.zu1");
    let rows = fx
        .conn
        .query("RETURN LET n = 6 IN n * 7 END AS n")
        .expect("query");
    let row = rows.iter().next().expect("one row");
    assert_eq!(row.get_by_name::<i64>("n").expect("n"), 42);
}

/// `LET` is a word here and a name everywhere else, the way every
/// context sensitive word in this grammar is: what makes it the
/// expression is the definition behind it.
#[test]
fn let_is_still_a_name() {
    let mut fx = Fixture::open("let-expr-name.zu1");
    let rows = fx
        .conn
        .query("LET let = 4 RETURN let + 1 AS id")
        .expect("query");
    let row = rows.iter().next().expect("one row");
    assert_eq!(row.get_by_name::<i64>("id").expect("id"), 5);
}

/// The word that closes the definitions is the word a membership test
/// is written with, and no reading gives both, so a test at the top of
/// a definition is written in parentheses. Inside them the word is the
/// operator again, which is the whole of the rule.
#[test]
fn a_membership_test_at_the_top_of_a_definition_goes_in_parentheses() {
    let mut fx = Fixture::open("let-expr-in.zu1");
    let rows = fx
        .conn
        .query("RETURN LET yes = (3 IN [1, 2, 3]) IN yes END AS yes")
        .expect("query");
    let row = rows.iter().next().expect("one row");
    assert!(row.get_by_name::<bool>("yes").expect("yes"));
    // The body is not a definition, so nothing is in parentheses there.
    let rows = fx
        .conn
        .query("RETURN LET n = 3 IN n IN [1, 2, 3] END AS yes")
        .expect("query");
    let row = rows.iter().next().expect("one row");
    assert!(row.get_by_name::<bool>("yes").expect("yes"));
}

/// A definition is worked out once however many times the body reads
/// it, which is the reason to write one rather than repeat the
/// expression. What is counted here is a call the engine cannot fold,
/// read twice out of a name defined once.
#[test]
fn a_definition_is_worked_out_once_however_often_it_is_read() {
    let mut fx = Fixture::open("let-expr-once.zu1");
    let ids = fx.ids(
        "MATCH (p:person) WHERE p.id < 3 \
         RETURN LET n = size([p.id, p.id]) IN n * n END AS id",
    );
    assert_eq!(ids, [4, 4, 4]);
}
