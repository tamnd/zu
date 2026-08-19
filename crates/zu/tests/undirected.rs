//! Undirected edges, and the edge patterns that walk them (GH02).
//!
//! ISO writes seven edge patterns and zu had three, because every rel
//! table was directed and the three spellings it read were the three a
//! directed table can answer. An undirected edge is one whose ends are
//! not a from and a to, and it is stored the way a directed one is,
//! once, with both stored lists answering for it. What that buys is the
//! pattern half: `~[]~` walks it from either end, the arrows refuse it,
//! and the two mixed spellings take it either way round.

use zu::query::{Value, run};
use zu_zu1::file::Zu1File;
use zu_zu1::graph::{bulk_load_as, bulk_load_undirected_as};

/// One undirected edge between two peers, which is the smallest graph
/// where the way round matters.
fn pair(dir: &std::path::Path) -> Zu1File {
    let mut zu = Zu1File::create(&dir.join("undirected.zu1")).unwrap();
    bulk_load_undirected_as(&mut zu, "peer", "friend", 2, &[(0, 1)]).unwrap();
    zu
}

/// The `id` values one query returns, sorted, so a pattern that walks
/// both stored lists is compared without minding which it read first.
fn ids(db: &mut Zu1File, source: &str) -> Vec<i64> {
    let result = run(source, db, &[]).unwrap_or_else(|e| panic!("{source}: {e}"));
    let mut out: Vec<i64> = result
        .rows
        .iter()
        .map(|row| match row[0] {
            Value::Int(i) => i,
            ref other => panic!("{source} returned {other:?}"),
        })
        .collect();
    out.sort_unstable();
    out
}

/// GH02. An undirected edge is written once and answers from both ends,
/// which is the whole of what the storage half has to do: the file
/// holds one edge, and the pattern finds it from either peer.
#[test]
fn an_undirected_edge_is_walked_from_either_end() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = pair(dir.path());
    let both = "MATCH (a:peer)~[:friend]~(b:peer) WHERE a.id = {} RETURN b.id AS id";
    assert_eq!(ids(&mut db, &both.replace("{}", "0")), [1]);
    assert_eq!(ids(&mut db, &both.replace("{}", "1")), [0]);

    // And the edge is one edge, not two: walking every peer's friends
    // finds the pair twice, once from each end, and no more.
    assert_eq!(
        ids(
            &mut db,
            "MATCH (a:peer)~[:friend]~(b:peer) RETURN b.id AS id"
        ),
        [0, 1]
    );
}

/// The four spellings that admit an undirected edge all walk it, and
/// the ones that ask for a direction find no table to walk at all. A
/// step no table fits walks nothing and the statement answers with no
/// rows, the same as any other pattern the graph cannot satisfy.
#[test]
fn the_arrows_refuse_an_undirected_edge_and_the_tildes_take_it() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = pair(dir.path());
    for pattern in ["~[:friend]~", "<~[:friend]~", "~[:friend]~>", "-[:friend]-"] {
        let source = format!("MATCH (a:peer){pattern}(b:peer) WHERE a.id = 0 RETURN b.id AS id");
        assert_eq!(ids(&mut db, &source), [1], "{pattern}");
    }
    for pattern in ["-[:friend]->", "<-[:friend]-", "<-[:friend]->"] {
        let source = format!("MATCH (a:peer){pattern}(b:peer) WHERE a.id = 0 RETURN b.id AS id");
        let rows = run(&source, &mut db, &[]).expect(&source).rows;
        assert!(
            rows.is_empty(),
            "{pattern} walked an undirected edge: {rows:?}"
        );
    }
}

/// The other way round: a directed table keeps the arrows and refuses
/// the tilde, so a file written before any of this reads the way it
/// always did.
#[test]
fn a_directed_edge_refuses_the_tilde() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = Zu1File::create(&dir.path().join("directed.zu1")).unwrap();
    bulk_load_as(&mut db, "peer", "friend", 2, &[(0, 1)]).unwrap();
    assert_eq!(
        ids(
            &mut db,
            "MATCH (a:peer)-[:friend]->(b:peer) WHERE a.id = 0 RETURN b.id AS id"
        ),
        [1]
    );
    assert_eq!(
        ids(
            &mut db,
            "MATCH (a:peer)-[:friend]-(b:peer) WHERE a.id = 1 RETURN b.id AS id"
        ),
        [0]
    );
    let rows = run(
        "MATCH (a:peer)~[:friend]~(b:peer) RETURN b.id AS id",
        &mut db,
        &[],
    )
    .expect("the statement runs")
    .rows;
    assert!(
        rows.is_empty(),
        "a directed table is no undirected one: {rows:?}"
    );
}

/// The abbreviated spellings, which drop the bracket and so drop the
/// type filter with it (ISO 39075 18.9). They are the same seven
/// patterns and they pick the same tables.
#[test]
fn the_abbreviated_spellings_are_the_same_patterns() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = pair(dir.path());
    for pattern in ["~", "<~", "~>", "-"] {
        let source = format!("MATCH (a:peer){pattern}(b:peer) WHERE a.id = 0 RETURN b.id AS id");
        assert_eq!(ids(&mut db, &source), [1], "{pattern}");
    }
}
