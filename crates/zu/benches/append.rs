//! What the bulk-load appender costs (dx/04 §6, §9).
//!
//! Two numbers, and they are the two halves of the appender's reason to
//! exist. The first is what buffering one row costs, because that is
//! the loop a loader is in and everything in it is per row: the tuple
//! is read into the column buffers with no vector in between, and an
//! allocation or a lookup that crept into that path would show up here
//! and nowhere else. The second is what batching buys, which is the
//! whole design: the same rows loaded with one flush against the same
//! rows loaded with a flush each, timed in one run on one file, so a
//! shared runner cannot move the ratio.
//!
//! Both are checked rather than only timed. Every load reads its rows
//! back through a query and compares them against what went in, so a
//! run that got faster by writing less fails instead of scoring.
//!
//! Run: ZU_GATE=1 cargo bench -p zu --bench append

use std::time::Instant;

use zu::query::Value;
use zu::zu1::file::Zu1File;
use zu::zu1::graph::bulk_load_as;
use zu::zu1::props::{PropValues, store_props};
use zu::{Config, Database};

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

/// Rows the seeded table starts with, small enough that the fold a
/// flush ends with is not what the batching ratio measures.
const SEED: u64 = 1_000;
/// Rows appended per measured load.
const ROWS: u64 = 2_000;
/// Rows buffered for the per row measurement, which flushes once and
/// only after the clock has stopped.
const BUFFERED: u64 = 200_000;

/// Builds a database with a two column `person` table and the
/// `follows` rel table over it.
fn build(path: &std::path::Path) {
    let mut db = Zu1File::create(path).expect("create");
    let edges: Vec<(u32, u32)> = (0..SEED as u32)
        .map(|i| (i, (i * 7 + 1) % SEED as u32))
        .collect();
    bulk_load_as(&mut db, "person", "follows", SEED, &edges).expect("load");
    let names: Vec<Vec<u8>> = (0..SEED).map(|i| format!("seed{i}").into_bytes()).collect();
    let refs: Vec<&[u8]> = names.iter().map(Vec::as_slice).collect();
    let ages: Vec<u64> = (0..SEED).collect();
    store_props(
        &mut db,
        "person",
        &[
            ("age", PropValues::Int(&ages)),
            ("name", PropValues::Str(&refs)),
        ],
    )
    .expect("props");
}

fn people(conn: &mut zu::Connection) -> i64 {
    let r = conn
        .query("MATCH (p:person) RETURN count(p) AS n")
        .expect("count");
    match r.rows.first().and_then(|row| row.first()) {
        Some(Value::Int(n)) => *n,
        other => panic!("expected a count, got {other:?}"),
    }
}

/// The per row cost of the buffering path, with no flush inside the
/// timed loop: every row of this load goes into memory, and the one
/// commit it has is paid after the clock stops.
fn run_append_row(path: &std::path::Path, names: &[String]) -> f64 {
    let db = Database::open_with(path, Config::new().threads(1)).expect("open");
    let mut conn = db.connect().expect("connect");
    let mut app = conn.appender("person").expect("appender");
    for i in 0..1_000u64 {
        app.append_row((i as i64, names[i as usize % names.len()].as_str()))
            .expect("warmup");
    }
    let start = Instant::now();
    for i in 0..BUFFERED {
        app.append_row((i as i64, names[i as usize % names.len()].as_str()))
            .expect("row");
    }
    let ns = start.elapsed().as_nanos() as f64 / BUFFERED as f64;
    assert_eq!(app.buffered(), BUFFERED + 1_000, "every row buffered");
    // Closed after the clock stops, so the one flush this load has is
    // outside the number, and so that the rows are checked rather than
    // dropped: a buffering path that quietly kept fewer rows than it
    // was given would commit fewer here.
    let committed = app.close().expect("close");
    assert_eq!(committed, BUFFERED + 1_000, "every row committed");
    assert_eq!(
        people(&mut conn),
        (SEED + BUFFERED + 1_000) as i64,
        "and every row readable"
    );
    ns
}

/// A whole load: `ROWS` rows appended and committed in one flush.
fn run_one_flush(path: &std::path::Path, names: &[String]) -> f64 {
    let db = Database::open_with(path, Config::new().threads(1)).expect("open");
    let mut conn = db.connect().expect("connect");
    let before = people(&mut conn);
    let start = Instant::now();
    let mut app = conn.appender("person").expect("appender");
    for i in 0..ROWS {
        app.append_row((i as i64, names[i as usize % names.len()].as_str()))
            .expect("row");
    }
    let committed = app.close().expect("close");
    let us = start.elapsed().as_nanos() as f64 / 1e3;
    assert_eq!(committed, ROWS);
    assert_eq!(
        people(&mut conn),
        before + ROWS as i64,
        "every row readable"
    );
    us
}

/// The same rows, one flush each, which is the shape a loader has when
/// it commits per row and the shape the appender exists to replace.
fn run_flush_each(path: &std::path::Path, names: &[String], rows: u64) -> f64 {
    let db = Database::open_with(path, Config::new().threads(1)).expect("open");
    let mut conn = db.connect().expect("connect");
    let before = people(&mut conn);
    let start = Instant::now();
    let mut app = conn.appender("person").expect("appender");
    for i in 0..rows {
        app.append_row((i as i64, names[i as usize % names.len()].as_str()))
            .expect("row");
        app.flush().expect("flush");
    }
    let committed = app.close().expect("close");
    let us = start.elapsed().as_nanos() as f64 / 1e3;
    assert_eq!(committed, rows);
    assert_eq!(
        people(&mut conn),
        before + rows as i64,
        "every row readable"
    );
    us
}

fn main() {
    let gate = std::env::var("ZU_GATE").is_ok_and(|v| v == "1");
    let dir = tempfile::tempdir().expect("tempdir");
    let names: Vec<String> = (0..64).map(|i| format!("person-{i}")).collect();

    let buffer_path = dir.path().join("buffer.zu1");
    build(&buffer_path);
    let row_ns = run_append_row(&buffer_path, &names);
    println!("append_row: {row_ns:.1} ns per row over {BUFFERED} rows, no flush");

    let one_path = dir.path().join("one.zu1");
    build(&one_path);
    let one_us = run_one_flush(&one_path, &names);

    // The per flush load runs over a tenth of the rows, because a flush
    // per row is quadratic in the table and the full count would take
    // minutes to say what a tenth says in seconds. The ratio is scaled
    // back up, which is generous to the per flush side: its own cost
    // per row grows with the table, so the real gap at ROWS is wider
    // than this number, never narrower.
    let each_rows = ROWS / 10;
    let each_path = dir.path().join("each.zu1");
    build(&each_path);
    let each_us = run_flush_each(&each_path, &names, each_rows) * 10.0;

    let batch_x = one_us / each_us.max(0.001);
    println!("{ROWS} rows, one flush:       {one_us:.0} us");
    println!("{ROWS} rows, a flush per row: {each_us:.0} us, scaled from {each_rows} rows");
    println!("batching: {batch_x:.4}x the cost of committing per row");

    let mut failed = false;
    if let Some(ceiling) = budget("append_row_ns")
        && row_ns > ceiling
    {
        println!("GATE FAIL append_row: {row_ns:.1} ns > ceiling {ceiling}");
        failed = true;
    }
    if let Some(ceiling) = budget("append_batch_x")
        && batch_x > ceiling
    {
        println!("GATE FAIL batching: {batch_x:.4}x > ceiling {ceiling}");
        failed = true;
    }
    if gate && failed {
        std::process::exit(1);
    }
    if failed {
        println!("gate: informational run, set ZU_GATE=1 to enforce");
    } else {
        println!("gate: all ceilings met");
    }
}
