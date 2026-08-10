//! P2 scaling gate (Spec/2064g/perf/03, zu#75): the pipeline executor
//! must scale across cores once a scan is wide enough to split into
//! many morsels.
//!
//! The graph is synthetic, one million people over eight storage
//! groups with four million knows edges and an age column, built
//! fresh in a tempdir. That width matters: the auto thread heuristic
//! keeps single-group scans sequential because forking snapshots
//! costs more than the scan, so the parallel path needs a table that
//! actually earns it. Two queries run at 1 and 8 threads through the
//! public zu::query::run path, a scan-filter-count and an
//! expand-filter-count whose per-row gathers give workers real work
//! beyond memory bandwidth. Every count is crosschecked against a
//! reference computed from the raw age formula and edge list, and
//! the 1-thread and 8-thread answers must agree exactly.
//!
//! exec_scale_8x gates the expand query's speedup at 8 workers over
//! one. The gate only arms on hosts with at least 8 cores; server1
//! has 4 and prints information numbers.
//!
//! Run: ZU_GATE=1 cargo bench -p zu --bench scale

use std::time::Instant;

use zu::query::{self, Value};
use zu::zu1::file::Zu1File;
use zu::zu1::graph::bulk_load_as;
use zu::zu1::props::{PropValues, store_props};

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

const NODES: u64 = 1 << 20;
const AGE_MOD: u64 = 1000;
const CUT: i64 = 499;

fn age_of(i: u64) -> u64 {
    (i * 37) % AGE_MOD
}

/// Builds the graph and returns the sorted deduped edge list for the
/// reference counts.
fn build(path: &std::path::Path) -> Vec<(u32, u32)> {
    let mut rng = 0x2064u64;
    let mut edges: Vec<(u32, u32)> = (0..NODES * 4)
        .map(|_| {
            let src = (xorshift(&mut rng) % NODES) as u32;
            let dst = (xorshift(&mut rng) % NODES) as u32;
            (src, dst)
        })
        .collect();
    edges.sort_unstable();
    edges.dedup();
    let mut db = Zu1File::create(path).expect("create");
    bulk_load_as(&mut db, "person", "knows", NODES, &edges).expect("load");
    let age: Vec<u64> = (0..NODES).map(age_of).collect();
    store_props(&mut db, "person", &[("age", PropValues::Int(&age))]).expect("props");
    edges
}

fn count_of(r: &zu::query::QueryResult) -> i64 {
    match r.rows.first().and_then(|row| row.first()) {
        Some(Value::Int(n)) => *n,
        other => panic!("expected one count, got {other:?}"),
    }
}

/// Median latency in ms of `source` at a fixed worker count, and the
/// count it returned on every run.
fn measure(db: &mut Zu1File, source: &str, threads: usize, runs: usize) -> (f64, i64) {
    // SAFETY: the bench main is single threaded when this is set;
    // workers only spawn inside the timed calls below.
    unsafe { std::env::set_var("ZU_THREADS", threads.to_string()) };
    let expect = count_of(&query::run(source, db, &[]).expect("warmup"));
    let mut times: Vec<f64> = (0..runs)
        .map(|_| {
            let t = Instant::now();
            let r = query::run(source, db, &[]).expect("timed run");
            let ms = t.elapsed().as_secs_f64() * 1e3;
            assert_eq!(count_of(&r), expect, "answer changed between runs");
            ms
        })
        .collect();
    times.sort_by(f64::total_cmp);
    (times[times.len() / 2], expect)
}

fn main() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("scale.zu1");
    let t = Instant::now();
    let edges = build(&path);
    let mut db = Zu1File::open(&path).expect("open");
    println!(
        "scale: {NODES} persons, {} knows edges, load {:.1}s",
        edges.len(),
        t.elapsed().as_secs_f64()
    );

    let scan_ref: i64 = (0..NODES).filter(|&i| age_of(i) as i64 > CUT).count() as i64;
    let expand_ref: i64 = edges
        .iter()
        .filter(|&&(_, dst)| age_of(u64::from(dst)) as i64 > CUT)
        .count() as i64;

    let scan_q = "MATCH (p:person) WHERE p.age > 499 RETURN count(p) AS n";
    let expand_q = "MATCH (a:person)-[:knows]->(b) WHERE b.age > 499 RETURN count(b) AS n";

    let (scan_1, c) = measure(&mut db, scan_q, 1, 9);
    assert_eq!(c, scan_ref, "scan count against the age formula");
    let (scan_8, c) = measure(&mut db, scan_q, 8, 9);
    assert_eq!(c, scan_ref, "parallel scan count against the age formula");
    println!(
        "scale scan-filter-count: 1 thread {scan_1:.2} ms, 8 threads {scan_8:.2} ms, {:.1}x, crosschecked",
        scan_1 / scan_8
    );

    let (exp_1, c) = measure(&mut db, expand_q, 1, 9);
    assert_eq!(c, expand_ref, "expand count against the edge list");
    let (exp_8, c) = measure(&mut db, expand_q, 8, 9);
    assert_eq!(c, expand_ref, "parallel expand count against the edge list");
    let scale = exp_1 / exp_8;
    println!(
        "scale expand-filter-count: 1 thread {exp_1:.2} ms, 8 threads {exp_8:.2} ms, {scale:.1}x, crosschecked"
    );

    let gate = std::env::var("ZU_GATE").as_deref() == Ok("1");
    let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
    if gate && cores >= 8 {
        if let Some(floor) = budget("exec_scale_8x") {
            assert!(
                scale >= floor,
                "exec2 8-worker scaling {scale:.2}x under the {floor}x floor"
            );
            println!("gate: scaling floor met");
        }
    } else if gate {
        println!("gate: {cores} cores, scaling floor needs 8, information only");
    }
}
