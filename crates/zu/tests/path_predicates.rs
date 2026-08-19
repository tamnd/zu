//! A `WHERE` written inside a relationship bracket, asked of every edge
//! the step stands on before it walks it.
//!
//! The point of putting it there rather than after the pattern is that
//! an edge which fails it is never walked, so no longer path is built
//! through it either. On a fixed length step that is only a cheaper
//! spelling of a filter. On a variable length step it is a different
//! answer: a two hop walk whose first edge fails the test does not
//! exist, and a filter written after the pattern cannot say that
//! without spelling out a quantifier over the list.
//!
//! The fixture is the finbench shape these queries come from: transfers
//! between accounts, each stamped with a time, and a question that only
//! wants the transfers inside a window.

use zu::convert::sqlite_to_zu1;
use zu::query::run;
use zu_query::exec::Value;
use zu_sqlite::{ColumnType, SqliteStore, Value as SqlValue};
use zu_zu1::file::Zu1File;

/// Four accounts and the transfers between them, each with a time.
///
/// 0 to 1 at 1, 1 to 2 at 9, 0 to 3 at 7, 3 to 2 at 8. So inside the
/// window 5 to 10 the only two hop walk is 0 to 3 to 2, and the walk
/// 0 to 1 to 2 exists only if the first hop is allowed to be outside
/// the window. That is the pair the gate has to tell apart.
const TRANSFERS: [(i64, i64, i64); 4] = [(0, 1, 1), (1, 2, 9), (0, 3, 7), (3, 2, 8)];

fn graph(dir: &std::path::Path) -> Zu1File {
    let sqlite = dir.join("fixture.db");
    let zu1 = dir.join("fixture.zu1");
    let mut sq = SqliteStore::open(&sqlite).unwrap();
    sq.create_node_table("account", &[]).unwrap();
    sq.create_rel_table(
        "transfer",
        "account",
        "account",
        &[("ts", ColumnType::Integer)],
    )
    .unwrap();
    sq.begin().unwrap();
    for row in 0..4i64 {
        sq.insert_node_at("account", row, &[]).unwrap();
    }
    for &(src, dst, ts) in &TRANSFERS {
        sq.insert_rel("transfer", src, dst, &[SqlValue::Int(ts)])
            .unwrap();
    }
    sq.commit().unwrap();
    drop(sq);
    sqlite_to_zu1(&sqlite, &zu1).unwrap();
    Zu1File::open(&zu1).unwrap()
}

fn rows(db: &mut Zu1File, source: &str) -> Vec<Vec<Value>> {
    run(source, db, &[])
        .unwrap_or_else(|e| panic!("{source}: {e}"))
        .rows
        .into_vec()
}

fn ints(db: &mut Zu1File, source: &str) -> Vec<i64> {
    rows(db, source)
        .into_iter()
        .map(|row| match row[0] {
            Value::Int(v) => v,
            ref other => panic!("{source} answered {other:?}"),
        })
        .collect()
}

/// On a fixed length step the gate says what a filter after the pattern
/// says, because there is nothing downstream of one edge to prune.
#[test]
fn one_hop_gated_matches_the_same_filter_written_after_the_pattern() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = graph(dir.path());
    let gated = ints(
        &mut db,
        "MATCH (a:account)-[t:transfer WHERE t.ts >= 7]->(b:account) \
         RETURN b.id AS id ORDER BY id",
    );
    let filtered = ints(
        &mut db,
        "MATCH (a:account)-[t:transfer]->(b:account) WHERE t.ts >= 7 \
         RETURN b.id AS id ORDER BY id",
    );
    assert_eq!(gated, filtered, "the gate and the filter agree on one hop");
    assert_eq!(gated, vec![2, 2, 3], "1 to 2, 3 to 2 and 0 to 3");
}

/// On a variable length step it prunes the walk. Ungated, both 1 and 3
/// reach 2 in two hops. Gated on the window, only the walk through 3
/// survives, because the first hop of the other one is at time 1.
#[test]
fn a_gated_walk_never_goes_through_an_edge_that_fails() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = graph(dir.path());
    let open = ints(
        &mut db,
        "MATCH (a:account)-[t:transfer*2..2]->(b:account) WHERE a.id = 0 \
         RETURN b.id AS id ORDER BY id",
    );
    assert_eq!(open, vec![2, 2], "0 reaches 2 twice with no window");

    let gated = ints(
        &mut db,
        "MATCH (a:account)-[t:transfer*2..2 WHERE t.ts >= 5 AND t.ts < 10]->(b:account) \
         WHERE a.id = 0 RETURN b.id AS id ORDER BY id",
    );
    assert_eq!(gated, vec![2], "only 0 to 3 to 2 is inside the window");
}

/// The variable names one edge inside the brackets and the list of them
/// outside, so the same name answers a property in the gate and a
/// length in the projection.
#[test]
fn the_variable_is_one_edge_inside_the_brackets_and_the_list_outside() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = graph(dir.path());
    let hops = ints(
        &mut db,
        "MATCH (a:account)-[t:transfer*1..3 WHERE t.ts >= 5]->(b:account) \
         WHERE a.id = 0 RETURN size(t) AS n ORDER BY n",
    );
    assert_eq!(
        hops,
        vec![1, 2],
        "0 to 3, and 0 to 3 to 2, both inside the window"
    );
}

/// A gate that nothing satisfies leaves the pattern with no rows rather
/// than with the ungated ones.
#[test]
fn a_gate_nothing_passes_empties_the_pattern() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = graph(dir.path());
    assert!(
        rows(
            &mut db,
            "MATCH (a:account)-[t:transfer*1..3 WHERE t.ts > 100]->(b:account) \
             RETURN b.id AS id",
        )
        .is_empty(),
        "no transfer is that late"
    );
}

/// The shortest walk under a gate is the shortest of the walks that
/// survive it, not the shortest walk with the failures removed
/// afterwards. Here 0 reaches 2 in two hops either way, so the gate has
/// to change which two hops, and the answer is the one through 3.
#[test]
fn any_shortest_takes_the_shortest_surviving_walk() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = graph(dir.path());
    let through = rows(
        &mut db,
        "MATCH ANY SHORTEST (a:account)-[t:transfer*1..4 WHERE t.ts >= 5]->(b:account) \
         WHERE a.id = 0 AND b.id = 2 RETURN size(t) AS n",
    );
    assert_eq!(through.len(), 1, "one shortest walk: {through:?}");
    assert_eq!(through[0][0], Value::Int(2), "0 to 3 to 2");
}

/// The gate is part of the walk, so both plan texts say so at the
/// operator that walks rather than at a filter after it.
#[test]
fn the_plan_text_shows_the_gate_on_the_expansion() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = graph(dir.path());
    let source = "MATCH (a:account)-[t:transfer*1..3 WHERE t.ts >= 5]->(b:account) \
         RETURN b.id AS id";
    let catalog = zu_zu1::catalog::Catalog::load(&mut db).expect("catalog");
    let logical = zu::query::explain(source, &catalog).expect("explain");
    assert!(
        logical.contains("[t:transfer*1..3 WHERE t.ts >= 5]"),
        "the gate belongs inside the brackets: {logical}"
    );
    let physical = zu::query::explain_analyze(source, &mut db, &[]).expect("explain analyze");
    assert!(
        physical.contains("VarExpand") && physical.contains("where t.ts >= 5"),
        "the gate belongs on the expansion: {physical}"
    );
}
