//! What the public API costs over the engine it wraps (dx/04 §9).
//!
//! Two numbers. The first is what taking a connection costs, because
//! the whole `Database`/`Connection` split rests on a connection being
//! cheap enough to take per thread, per request, or per pool checkout:
//! if connecting cost what opening a database costs, every binding
//! above this would build a pool to avoid it and the split would have
//! bought nothing. The second is what a statement costs through
//! `Connection` against the same statement through `Session`, since a
//! wrapper that adds latency to the hot path is a wrapper nobody
//! serious will use.
//!
//! The graph is the same synthetic one the session gates use, built
//! fresh in a tempdir: these are query-plane numbers and the point read
//! touches one key lookup and one adjacency list at any graph size.
//! Every timed read is crosschecked against a degree table computed
//! from the edge list, so a run that got faster by answering wrongly
//! fails instead of scoring.
//!
//! connect_us gates Database::connect. api_overhead_x gates the ratio
//! of the warm point read through a connection to the same read through
//! a session, and it is the number that says this API is free.
//!
//! Run: ZU_GATE=1 cargo bench -p zu --bench connect

use std::time::Instant;

use zu::query::Value;
use zu::session::Session;
use zu::zu1::file::Zu1File;
use zu::zu1::graph::bulk_load_as;
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

fn xorshift(rng: &mut u64) -> u64 {
    *rng ^= *rng << 13;
    *rng ^= *rng >> 7;
    *rng ^= *rng << 17;
    *rng
}

const NODES: u32 = 10_000;
const POINT_Q: &str = "MATCH (a:person {id: $src})-[:follows]->(b) RETURN count(b) AS n";

/// Builds the synthetic graph and returns out-degree by node, the
/// reference every timed read checks against.
fn build(path: &std::path::Path) -> Vec<i64> {
    let mut rng = 0x2064u64;
    let mut edges: Vec<(u32, u32)> = (0..NODES * 4)
        .map(|_| {
            let src = (xorshift(&mut rng) % u64::from(NODES)) as u32;
            let dst = (xorshift(&mut rng) % u64::from(NODES)) as u32;
            (src, dst)
        })
        .collect();
    edges.sort_unstable();
    edges.dedup();
    let mut db = Zu1File::create(path).expect("create");
    bulk_load_as(&mut db, "person", "follows", u64::from(NODES), &edges).expect("load");
    let mut degree = vec![0i64; NODES as usize];
    for (src, _) in &edges {
        degree[*src as usize] += 1;
    }
    degree
}

fn count_of(r: &zu::query::QueryResult) -> i64 {
    match r.rows.first().and_then(|row| row.first()) {
        Some(Value::Int(n)) => *n,
        other => panic!("expected one count, got {other:?}"),
    }
}

fn p50(mut lat: Vec<u64>) -> f64 {
    lat.sort_unstable();
    lat[lat.len() / 2] as f64 / 1e3
}

/// Median cost of taking a connection: an open, a header read, and a
/// catalog load. The connections are dropped as they are taken, which
/// is the shape a request handler has.
fn run_connect(db: &Database) -> f64 {
    for _ in 0..50 {
        drop(db.connect().expect("warmup"));
    }
    let mut lat: Vec<u64> = Vec::with_capacity(2_000);
    for _ in 0..2_000 {
        let start = Instant::now();
        let conn = db.connect().expect("connect");
        lat.push(start.elapsed().as_nanos() as u64);
        drop(conn);
    }
    p50(lat)
}

/// Median cost of opening the database itself, for contrast: it is the
/// same work plus the validating open, and it is paid once per process
/// rather than once per connection.
fn run_open(path: &std::path::Path) -> f64 {
    let mut lat: Vec<u64> = Vec::with_capacity(500);
    for _ in 0..500 {
        let start = Instant::now();
        let db = Database::open(path).expect("open");
        lat.push(start.elapsed().as_nanos() as u64);
        drop(db);
    }
    p50(lat)
}

/// Median latency of the warm point read through a connection.
fn run_connection_point(db: &Database, degree: &[i64]) -> f64 {
    let mut conn = db.connect().expect("connect");
    let mut rng = 0xbeefu64;
    for _ in 0..100 {
        let src = (xorshift(&mut rng) % u64::from(NODES)) as i64;
        conn.query_with(POINT_Q, &[("src", Value::Int(src))])
            .expect("warmup");
    }
    let mut lat: Vec<u64> = Vec::with_capacity(10_000);
    for _ in 0..10_000 {
        let src = (xorshift(&mut rng) % u64::from(NODES)) as i64;
        let start = Instant::now();
        let r = conn
            .query_with(POINT_Q, &[("src", Value::Int(src))])
            .expect("point");
        lat.push(start.elapsed().as_nanos() as u64);
        assert_eq!(count_of(&r), degree[src as usize], "src {src}");
    }
    p50(lat)
}

/// The same read through the engine's own session, the baseline the
/// wrapper is measured against. Same seed, so both paths read the same
/// sequence of nodes.
fn run_session_point(path: &std::path::Path, degree: &[i64]) -> f64 {
    let mut session = Session::open(path).expect("open");
    let mut rng = 0xbeefu64;
    for _ in 0..100 {
        let src = (xorshift(&mut rng) % u64::from(NODES)) as i64;
        session
            .run(POINT_Q, &[("src", Value::Int(src))])
            .expect("warmup");
    }
    let mut lat: Vec<u64> = Vec::with_capacity(10_000);
    for _ in 0..10_000 {
        let src = (xorshift(&mut rng) % u64::from(NODES)) as i64;
        let start = Instant::now();
        let r = session
            .run(POINT_Q, &[("src", Value::Int(src))])
            .expect("point");
        lat.push(start.elapsed().as_nanos() as u64);
        assert_eq!(count_of(&r), degree[src as usize], "src {src}");
    }
    p50(lat)
}

fn main() {
    let gate = std::env::var("ZU_GATE").is_ok_and(|v| v == "1");
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("connect.zu1");
    let degree = build(&path);
    let edge_count: i64 = degree.iter().sum();
    println!("connect bench graph: {NODES} nodes, {edge_count} edges");

    // A one-thread configuration, so the point read is the same work on
    // both paths and the ratio measures the wrapper rather than which
    // path happened to get the machine's cores.
    let db = Database::open_with(&path, Config::new().threads(1)).expect("open");
    let open_us = run_open(&path);
    let connect_us = run_connect(&db);
    println!("Database::open: p50 {open_us:.2} us over 500 opens");
    println!(
        "Database::connect: p50 {connect_us:.2} us over 2000 connections, \
         {ratio:.2}x an open",
        ratio = connect_us / open_us.max(0.001)
    );

    let connection_us = run_connection_point(&db, &degree);
    let session_us = run_session_point(&path, &degree);
    let overhead_x = connection_us / session_us.max(0.001);
    println!("point read through a connection: p50 {connection_us:.2} us over 10000 reads");
    println!("point read through a session:    p50 {session_us:.2} us over 10000 reads");
    println!("API overhead: {overhead_x:.3}x the session path");

    let mut failed = false;
    if let Some(ceiling) = budget("connect_us")
        && connect_us > ceiling
    {
        println!("GATE FAIL Database::connect: p50 {connect_us:.2} us > ceiling {ceiling}");
        failed = true;
    }
    if let Some(ceiling) = budget("api_overhead_x")
        && overhead_x > ceiling
    {
        println!("GATE FAIL API overhead: {overhead_x:.3}x > ceiling {ceiling}");
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
