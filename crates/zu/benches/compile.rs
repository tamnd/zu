//! The cold statement: a text the session has never seen (zu#658).
//!
//! Everything else about the query plane is measured warm, because a
//! client that sends the same statement a million times pays the plan
//! cache once and the cache hit the other 999999 times. A cold send is
//! the other half of that, and it is what a shell session, a
//! conformance corpus and the first minute of any workload are made
//! of: every text is new, so every text is parsed, bound, planned and
//! optimised.
//!
//! What this bench exists to hold is the number of parses in a cold
//! send. It used to be three of the same text: once to decide the text
//! was a query rather than one of the four things that is not, once to
//! read the `USE` in front of it, and once to compile. #658 carries
//! the tree through instead, so it is one.
//!
//! Three numbers are printed:
//!
//! - a cold miss through a session, which is the whole compile
//! - a cold miss through a connection, and the same through a
//!   read-only one, which pays one parse more than that for the
//!   refusal that sits in front of it
//! - the warm hit on the same session, for scale
//!
//! Nothing here is gated. A compile is a cost that depends on the
//! shape of the statement and the size of the catalog, so a ceiling in
//! microseconds would be a statement about this graph and no other.
//! The numbers are printed and compared across a change.
//!
//! Run: cargo bench -p zu --bench compile

use std::time::Instant;

use zu::db::{Config, Database};
use zu::query::Value;
use zu::session::Session;
use zu::zu1::file::Zu1File;
use zu::zu1::graph::bulk_load_as;

fn xorshift(rng: &mut u64) -> u64 {
    *rng ^= *rng << 13;
    *rng ^= *rng >> 7;
    *rng ^= *rng << 17;
    *rng
}

const NODES: u32 = 10_000;

/// Builds the same synthetic graph the session bench uses, so the two
/// sets of numbers are about one graph.
fn build(path: &std::path::Path) -> usize {
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
    edges.len()
}

/// A text nothing has seen before.
///
/// The alias carries the index, so the statements differ in one token
/// and are the same to parse, bind and plan. A cold send is only cold
/// once, and the plan cache is keyed on the text, so this is how a
/// thousand cold sends are had from one session.
fn cold_text(i: usize) -> String {
    format!("MATCH (a:person {{id: $src}})-[:follows]->(b) RETURN count(b) AS n{i}")
}

const SENDS: usize = 2_000;

/// Median of a cold compile through a session, the statement never run.
///
/// `warm` is the compile and nothing after it, which is what #658
/// changes: the executor is the same either side of it.
fn run_cold_compile(path: &std::path::Path) -> f64 {
    let mut session = Session::open(path).expect("open");
    // One send off the clock, so the catalog and the stats this session
    // reads are read before anything is timed.
    session.warm(&cold_text(usize::MAX)).expect("warmup");
    let mut lat: Vec<u64> = Vec::with_capacity(SENDS);
    for i in 0..SENDS {
        let text = cold_text(i);
        let start = Instant::now();
        let hit = session.warm(&text).expect("compile");
        lat.push(start.elapsed().as_nanos() as u64);
        assert!(!hit, "a text nothing has seen is a miss");
    }
    lat.sort_unstable();
    lat[lat.len() / 2] as f64 / 1e3
}

/// Median of a cold send through a connection, run and all.
///
/// A read-only connection refuses a statement that writes before it
/// compiles it, and deciding that is a parse of the text that the
/// session below then makes again. The read-write connection has no
/// such refusal, so the two numbers together say what that parse
/// costs.
fn run_cold_connection(path: &std::path::Path, read_only: bool) -> f64 {
    let db = Database::open_with(path, Config::new().read_only(read_only)).expect("open");
    let mut conn = db.connect().expect("connect");
    let mut rng = 0xbeefu64;
    conn.query_with(&cold_text(usize::MAX), &[("src", Value::Int(1))])
        .expect("warmup");
    let mut lat: Vec<u64> = Vec::with_capacity(SENDS);
    for i in 0..SENDS {
        let text = cold_text(i);
        let src = (xorshift(&mut rng) % u64::from(NODES)) as i64;
        let start = Instant::now();
        conn.query_with(&text, &[("src", Value::Int(src))])
            .expect("a read");
        lat.push(start.elapsed().as_nanos() as u64);
    }
    lat.sort_unstable();
    lat[lat.len() / 2] as f64 / 1e3
}

/// The warm hit on the same session, which is what a cold send is
/// being read against.
fn run_warm_hit(path: &std::path::Path) -> f64 {
    let mut session = Session::open(path).expect("open");
    let text = cold_text(0);
    assert!(!session.warm(&text).expect("compile"), "first sight");
    let mut lat: Vec<u64> = Vec::with_capacity(10_000);
    for _ in 0..10_000 {
        let start = Instant::now();
        let hit = session.warm(&text).expect("warm");
        lat.push(start.elapsed().as_nanos() as u64);
        assert!(hit, "the text stayed cached");
    }
    lat.sort_unstable();
    lat[lat.len() / 2] as f64 / 1e3
}

fn main() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("compile.zu1");
    let edges = build(&path);
    println!("compile bench graph: {NODES} nodes, {edges} edges");

    let cold_us = run_cold_compile(&path);
    println!("cold compile through a session: p50 {cold_us:.2} us over {SENDS} misses");
    let write_us = run_cold_connection(&path, false);
    println!("cold send on a connection: p50 {write_us:.2} us over {SENDS} misses");
    let read_only_us = run_cold_connection(&path, true);
    println!(
        "cold send on a read-only connection: p50 {read_only_us:.2} us, \
         {over:.2} us over the connection above",
        over = read_only_us - write_us
    );
    let warm_us = run_warm_hit(&path);
    println!(
        "warm plan cache hit: p50 {warm_us:.2} us, which the cold compile is \
         {ratio:.0}x",
        ratio = cold_us / warm_us.max(0.001)
    );
}
