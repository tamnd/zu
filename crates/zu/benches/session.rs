//! P0 session gates (Spec/2064g/perf/07, zu#73): a warm plan-cache
//! hit and a warm point read through a resident session.
//!
//! The graph is synthetic (10k people, ~40k follows edges, built
//! fresh in a tempdir), because these gates measure the query plane,
//! not the storage engine: the point read touches one key lookup and
//! one adjacency list whatever the graph size. Every timed query is
//! crosschecked against a reference computed from the edge list, and
//! the session numbers are printed next to the one-shot
//! open-load-plan-run path on the same file so the run shows what the
//! resident catalog and plan cache actually buy.
//!
//! session_plan_hit_us gates Session::warm on cached text, the work
//! between receiving a known query and starting execution.
//! session_point_us gates the full warm point read: plan hit,
//! parameter bind, key lookup, one-hop count, result row out.
//! session_point_held_us gates the same read on a session that is
//! holding session parameters, which is the G8 question: a session
//! that has set something is a session that has to fold what it holds
//! into what the caller passed on every statement, and the gate is
//! there so that cost stays a fold over a small map rather than
//! becoming a per statement allocation nobody measured.
//!
//! Each of the three has a `_p99_us` companion, because a serving
//! path is judged on the read that was slow and not on the middle one.
//! A median holds still through a cache that starts missing one time
//! in fifty, and that is exactly the regression these gates exist to
//! catch.
//!
//! Run: ZU_GATE=1 cargo bench -p zu --bench session

use std::time::Instant;

use zu::query::Value;
use zu::session::Session;
use zu::zu1::file::Zu1File;
use zu::zu1::graph::bulk_load_as;

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

/// The p50 and p99 of a latency sample, in microseconds. The sample is
/// sorted in place because every caller is done with the order.
fn pcts(lat: &mut [u64]) -> (f64, f64) {
    lat.sort_unstable();
    let at = |q: f64| lat[((lat.len() as f64 * q) as usize).min(lat.len() - 1)] as f64 / 1e3;
    (at(0.50), at(0.99))
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

/// Latency of Session::warm on already-cached text.
fn run_plan_hit(path: &std::path::Path) -> (f64, f64) {
    let mut session = Session::open(path).expect("open");
    assert!(!session.warm(POINT_Q).expect("compile"), "first sight");
    let mut lat: Vec<u64> = Vec::with_capacity(10_000);
    for _ in 0..10_000 {
        let start = Instant::now();
        let hit = session.warm(POINT_Q).expect("warm");
        lat.push(start.elapsed().as_nanos() as u64);
        assert!(hit, "text stayed cached");
    }
    pcts(&mut lat)
}

/// Latency of the full warm point read through the session,
/// random sources, every answer checked against the degree table.
fn run_session_point(path: &std::path::Path, degree: &[i64]) -> (f64, f64) {
    let mut session = Session::open(path).expect("open");
    let mut rng = 0xbeefu64;
    // Warm the plan cache and the reader caches before timing.
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
    pcts(&mut lat)
}

/// The same point read on a session holding three session parameters,
/// one of each kind (G8, GS01 through GS03).
///
/// Nothing the query reads comes from them. That is the point: what
/// is being measured is what a statement pays for a session having
/// state at all, which is the fold of the held map over the passed
/// list and the reference check that goes with it.
fn run_session_point_held(path: &std::path::Path, degree: &[i64]) -> (f64, f64) {
    let mut session = Session::open(path).expect("open");
    session
        .run("SESSION SET VALUE $cut = 35", &[])
        .expect("a value parameter");
    session
        .run(
            "SESSION SET PROPERTY GRAPH $g = CURRENT_PROPERTY_GRAPH",
            &[],
        )
        .expect("a graph parameter");
    session
        .run("SESSION SET BINDING TABLE $t = { RETURN 1 AS one }", &[])
        .expect("a binding table parameter");
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
    pcts(&mut lat)
}

/// The same point read through the one-shot path a bare `zu query`
/// pays: open, load catalog and stats, parse, plan, run. Printed for
/// contrast, not gated; the CLI's spawn cost is not even included.
fn run_one_shot_point(path: &std::path::Path, degree: &[i64]) -> (f64, f64) {
    let mut rng = 0xfeedu64;
    let mut lat: Vec<u64> = Vec::with_capacity(200);
    for _ in 0..200 {
        let src = (xorshift(&mut rng) % u64::from(NODES)) as i64;
        let start = Instant::now();
        let mut db = Zu1File::open(path).expect("open");
        let r = zu::query::run(POINT_Q, &mut db, &[("src", Value::Int(src))]).expect("one shot");
        lat.push(start.elapsed().as_nanos() as u64);
        assert_eq!(count_of(&r), degree[src as usize], "src {src}");
    }
    pcts(&mut lat)
}

fn main() {
    let gate = std::env::var("ZU_GATE").is_ok_and(|v| v == "1");
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("session.zu1");
    let degree = build(&path);
    let edge_count: i64 = degree.iter().sum();
    println!("session bench graph: {NODES} nodes, {edge_count} edges");

    let (plan_hit_us, plan_hit_p99) = run_plan_hit(&path);
    println!(
        "plan cache hit: p50 {plan_hit_us:.2} us, p99 {plan_hit_p99:.2} us over 10000 warm hits"
    );
    let (point_us, point_p99) = run_session_point(&path, &degree);
    println!(
        "session point read: p50 {point_us:.2} us, p99 {point_p99:.2} us over 10000 warm reads"
    );
    let (held_us, held_p99) = run_session_point_held(&path, &degree);
    println!(
        "session point read holding three parameters: p50 {held_us:.2} us, \
         p99 {held_p99:.2} us, {over:+.2} us over the empty session",
        over = held_us - point_us
    );
    let (one_shot_us, one_shot_p99) = run_one_shot_point(&path, &degree);
    println!(
        "one-shot point read (open+plan+run, no spawn): p50 {one_shot_us:.2} us, \
         p99 {one_shot_p99:.2} us, {ratio:.0}x the session path",
        ratio = one_shot_us / point_us.max(0.01)
    );

    let mut failed = false;
    // perf/13 flatness: a class gate gets a companion ratio, because a
    // p50 that improved while the tail doubled is not an improvement.
    // Five, because this bench builds its own graph with a uniform
    // degree and there is no skew here for a tail to come from. The
    // SF1 benches run on a real social graph and get ten.
    let flat = |what: &str, p50: f64, p99: f64| {
        let Some(ceiling) = budget("uniform_flatness_x") else {
            return false;
        };
        let ratio = p99 / p50.max(0.001);
        println!("{what} flatness: p99 is {ratio:.2}x the p50");
        if ratio > ceiling {
            println!("GATE FAIL {what} flatness: {ratio:.2}x > ceiling {ceiling}");
            return true;
        }
        false
    };
    failed |= flat("plan cache hit", plan_hit_us, plan_hit_p99);
    failed |= flat("session point read", point_us, point_p99);
    failed |= flat("held session point read", held_us, held_p99);
    failed |= flat("one-shot point read", one_shot_us, one_shot_p99);
    let mut check = |what: &str, pct: &str, key: &str, got: f64| {
        if let Some(ceiling) = budget(key)
            && got > ceiling
        {
            println!("GATE FAIL {what}: {pct} {got:.2} us > ceiling {ceiling}");
            failed = true;
        }
    };
    check("plan cache hit", "p50", "session_plan_hit_us", plan_hit_us);
    check(
        "plan cache hit",
        "p99",
        "session_plan_hit_p99_us",
        plan_hit_p99,
    );
    check("session point read", "p50", "session_point_us", point_us);
    check(
        "session point read",
        "p99",
        "session_point_p99_us",
        point_p99,
    );
    check(
        "held session point read",
        "p50",
        "session_point_held_us",
        held_us,
    );
    check(
        "held session point read",
        "p99",
        "session_point_held_p99_us",
        held_p99,
    );
    if gate && failed {
        std::process::exit(1);
    }
    if failed {
        println!("gate: informational run, set ZU_GATE=1 to enforce");
    } else {
        println!("gate: all ceilings met");
    }
}
