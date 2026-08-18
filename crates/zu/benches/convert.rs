//! What `zu convert` costs per node (Spec/2064g/gql/plan/09 section 4.3,
//! zu#116).
//!
//! Ingest is the one figure in the performance contract where zu was not
//! near ten times its nearest rival. Edges are fine: the loader takes a
//! sorted edge list and writes a CSR, and that path runs in the millions
//! per second. Nodes were not, and the contract says so out loud and
//! sets a target of 350 000 nodes/s, ten times the 35 359 Neo4j reaches
//! over Bolt.
//!
//! What is measured here is the route the conformance harness uses and
//! the only route into zu that carries labels and properties: a SQLite
//! database laid out the way zu's own SQLite engine lays one out, read
//! whole and written as a zu1 file. The staging write is not in the
//! number, because staging is scaffolding the harness put there and not
//! something an engine pays for in use, which is the same line the
//! harness draws.
//!
//! The fixture is the shape of the contract's own worst case: a million
//! nodes carrying two properties, joined in one long path, so the per
//! node cost is what the clock sees and the edge rate cannot hide it.
//! The conversion is checked rather than only timed. Every run reads the
//! node count, an edge count and a property back out of the file it
//! wrote, so a conversion that got faster by writing less fails here
//! instead of scoring.
//!
//! Run: ZU_GATE=1 cargo bench -p zu --bench convert

use std::time::Instant;

use zu::query::Value;
use zu::zu1::file::Zu1File;
use zu_sqlite::{ColumnType, SqliteStore, Value as SqlValue};

fn budget(key: &str) -> Option<f64> {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../bench/budgets.toml");
    for line in std::fs::read_to_string(path).ok()?.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        if let Some((k, v)) = line.split_once('=')
            && k.trim() == key
        {
            return v.trim().parse().ok();
        }
    }
    None
}

/// Nodes in the fixture, the size the contract quotes its zu figure at.
const NODES: i64 = 1_000_000;

/// Writes the staging database: `NODES` people on a path, each with an
/// integer and a string property.
fn stage(path: &std::path::Path) {
    let mut sq = SqliteStore::open(path).expect("open staging");
    sq.create_node_table(
        "person",
        &[("id", ColumnType::Integer), ("name", ColumnType::Text)],
    )
    .expect("node table");
    sq.create_rel_table("knows", "person", "person", &[])
        .expect("rel table");
    sq.begin().expect("begin");
    // At an explicit row rather than at a fresh rowid: the zu layer
    // owns dense row assignment, because a zu1 row domain is what a
    // node's place in the load is, and a staging file written any other
    // way is not the file a conversion reads.
    for row in 0..NODES {
        sq.insert_node_at(
            "person",
            row,
            &[SqlValue::Int(row), SqlValue::Text(format!("person-{row}"))],
        )
        .expect("node");
    }
    for row in 1..NODES {
        sq.insert_rel("knows", row - 1, row, &[]).expect("edge");
    }
    sq.commit().expect("commit");
    sq.checkpoint().expect("checkpoint");
}

/// Reads the converted file back, and panics unless it holds the graph
/// that went in.
fn check(path: &std::path::Path) {
    let mut db = Zu1File::open(path).expect("open converted");
    let count = |db: &mut Zu1File, source: &str| -> i64 {
        let r = zu::query::run(source, db, &[]).expect(source);
        match r.rows.first().and_then(|row| row.first()) {
            Some(Value::Int(n)) => *n,
            other => panic!("{source}: expected a count, got {other:?}"),
        }
    };
    assert_eq!(
        count(&mut db, "MATCH (p:person) RETURN count(p) AS n"),
        NODES,
        "every node"
    );
    assert_eq!(
        count(
            &mut db,
            "MATCH (:person)-[:knows]->(:person) RETURN count(*) AS n"
        ),
        NODES - 1,
        "every edge"
    );
    // A property read of the last row, which is the row a conversion
    // that stopped early would be missing.
    let r = zu::query::run(
        "MATCH (p:person) WHERE p.id = 999999 RETURN p.name AS name",
        &mut db,
        &[],
    )
    .expect("property");
    assert_eq!(
        r.rows.first().and_then(|row| row.first()),
        Some(&Value::Str("person-999999".into())),
        "and every property"
    );
}

fn main() {
    let gate = std::env::var("ZU_GATE").is_ok_and(|v| v == "1");
    let dir = tempfile::tempdir().expect("tempdir");
    let staging = dir.path().join("stage.db");
    let t = Instant::now();
    stage(&staging);
    println!(
        "convert: {NODES} nodes and {} edges staged in {:.1}s, which is not measured",
        NODES - 1,
        t.elapsed().as_secs_f64()
    );

    let out = dir.path().join("converted.zu1");
    let t = Instant::now();
    zu::convert::sqlite_to_zu1(&staging, &out).expect("convert");
    let secs = t.elapsed().as_secs_f64();
    check(&out);
    let nodes_s = NODES as f64 / secs;
    let edges_s = (NODES - 1) as f64 / secs;
    let bytes = std::fs::metadata(&out).expect("metadata").len();
    println!(
        "convert: {secs:.2}s, {nodes_s:.0} nodes/s, {edges_s:.0} edges/s, \
         {:.1} MiB written, crosschecked",
        bytes as f64 / (1024.0 * 1024.0)
    );

    let mut failed = false;
    if let Some(floor) = budget("convert_nodes_s")
        && nodes_s < floor
    {
        println!("GATE FAIL convert: {nodes_s:.0} nodes/s < floor {floor}");
        failed = true;
    }
    if gate && failed {
        std::process::exit(1);
    }
    if failed {
        println!("gate: informational run, set ZU_GATE=1 to enforce");
    } else {
        println!("gate: the floor is met");
    }
}
