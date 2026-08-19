//! The order by and page statement (ISO 14.9, features GQ12 and GQ13).
//!
//! `OFFSET` is the standard's word for what Cypher spells `SKIP` and
//! `LIMIT` is the same word in both, so what is checked here is that
//! the two spellings are one clause, that a window taken with both
//! ends is the window it looks like, and that a page of an ordered
//! result is a page rather than an arbitrary handful of rows.

use zu::Database;
use zu::zu1::file::Zu1File;
use zu::zu1::graph::bulk_load_as;

const NODES: u32 = 8;

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

    /// The `id` column in the order the query answered it, since a page
    /// of an ordered result is about the order it came in.
    fn ids(&mut self, source: &str) -> Vec<i64> {
        let rows = self.conn.query(source).expect("query");
        rows.iter()
            .map(|row| row.get_by_name::<i64>("id").expect("an integer column"))
            .collect()
    }
}

/// OFFSET is the standard's spelling and SKIP is the synonym, so a
/// query written either way answers the same page.
#[test]
fn offset_and_skip_are_one_clause() {
    let mut fx = Fixture::open("offset.zu1");
    let offset = fx.ids("MATCH (p:person) RETURN p.id AS id ORDER BY id OFFSET 5");
    assert_eq!(offset, [5, 6, 7]);
    let skip = fx.ids("MATCH (p:person) RETURN p.id AS id ORDER BY id SKIP 5");
    assert_eq!(offset, skip);
}

/// The two ends together take a window, which is the shape the
/// standard's order by and page statement has.
#[test]
fn an_offset_and_a_limit_take_a_window() {
    let mut fx = Fixture::open("window.zu1");
    assert_eq!(
        fx.ids("MATCH (p:person) RETURN p.id AS id ORDER BY id OFFSET 2 LIMIT 3"),
        [2, 3, 4]
    );
    assert_eq!(
        fx.ids("MATCH (p:person) RETURN p.id AS id ORDER BY id DESC OFFSET 1 LIMIT 2"),
        [6, 5],
        "the page is taken from the order the statement asked for"
    );
    assert_eq!(
        fx.ids("MATCH (p:person) RETURN p.id AS id ORDER BY id OFFSET 20"),
        Vec::<i64>::new(),
        "a page past the end is empty rather than the last page"
    );
}

/// Writing both words is writing one clause twice, so it is refused by
/// saying they are the same clause rather than by taking one of them.
#[test]
fn writing_both_words_is_refused() {
    let mut fx = Fixture::open("both.zu1");
    let err = fx
        .conn
        .query("MATCH (p:person) RETURN p.id AS id ORDER BY id OFFSET 1 SKIP 1")
        .expect_err("one clause, written twice");
    assert!(
        err.to_string().contains("two spellings of one clause"),
        "{err}, want the reason"
    );
}

/// A page is a page of what the statement it stands in answered, so a
/// window taken in the statement before a NEXT is what the statement
/// after it reads.
#[test]
fn a_page_is_taken_where_it_is_written() {
    let mut fx = Fixture::open("page-chain.zu1");
    let ids = fx.ids(
        "MATCH (p:person) RETURN p.id AS id ORDER BY id OFFSET 2 LIMIT 3 \
         NEXT FILTER id > 2 RETURN id AS id ORDER BY id",
    );
    assert_eq!(ids, [3, 4], "the filter reads the window, not the table");
}

/// The order by and page statement standing where a statement stands,
/// which is what ISO 14.9 says it is: the words are a statement of
/// their own and not a tail on the RETURN in front of them.
#[test]
fn the_page_stands_on_its_own() {
    let mut fx = Fixture::open("standalone.zu1");
    assert_eq!(
        fx.ids("MATCH (p:person) ORDER BY p.id DESC LIMIT 3 RETURN p.id AS id"),
        [7, 6, 5],
        "the rows were ordered and paged before the projection saw them"
    );
    assert_eq!(
        fx.ids("MATCH (p:person) ORDER BY p.id OFFSET 6 RETURN p.id AS id"),
        [6, 7]
    );
    assert_eq!(
        fx.ids("MATCH (p:person) LIMIT 2 RETURN p.id AS id").len(),
        2,
        "a LIMIT alone is a whole statement"
    );
}

/// The standalone statement orders the rows the walk bound rather than
/// the columns a projection made, so it sorts by things no column
/// holds.
#[test]
fn the_standalone_page_reads_what_is_bound() {
    let mut fx = Fixture::open("standalone-scope.zu1");
    assert_eq!(
        fx.ids(
            "MATCH (p:person)-[:knows]->(q:person) ORDER BY q.id DESC LIMIT 2 RETURN p.id AS id"
        ),
        [6, 5],
        "the key is a variable the projection does not carry"
    );
}
