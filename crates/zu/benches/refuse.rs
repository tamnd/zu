//! The refusal path against the answer path (G0, plan/08 section 1.2).
//!
//! Most of what a conformance corpus sends an engine at this stage of
//! its life is not a query it can run. The harness of 2026-08-12 put
//! 377 cases to zu and 156 of them ended in a refusal, so the cost of
//! saying no is a real part of what the run measures, and it is the
//! only part a user never asked for. An engine that spends more on a
//! statement it rejects than on one it answers is doing work it throws
//! away, and it is offering a client that sends nonsense a cheaper way
//! to spend its cores than a client that sends work.
//!
//! So the gate is stated as a comparison rather than as a number: every
//! statement zu refuses before executing it has to cost less than the
//! answer the same session gives to a statement it accepts. The answer
//! side is the warm point read the session bench gates, plan cache hit
//! and all, which is the fastest real answer this engine has. Refusing
//! has to beat that.
//!
//! Three refusals are timed, one per stage the statement can die at:
//!
//! - syntax, killed in the parser before anything is bound
//! - an undefined reference, killed after the parse and before planning
//! - a missing parameter, killed at bind time on a plan that is cached
//!   and correct, which is the cheapest refusal the engine can make
//!
//! A fourth, division by zero on a keyed read, is printed and not
//! gated: it is raised by the expression evaluator with rows already in
//! flight, so it costs a lookup plus the check by construction, and
//! holding it to the same rule would be asking the engine to stop
//! executing before it executes. It is printed because a runtime
//! refusal costing many times an answer would mean the check itself is
//! expensive.
//!
//! Every timed refusal asserts its GQLSTATUS, because a refusal that
//! stopped being the refusal we meant to time is not a faster refusal.
//!
//! There is a second gate under the comparison, an absolute ceiling in
//! microseconds, and it is there to catch the run where both paths got
//! slower together and the ratio held. A number of microseconds is a
//! statement about a machine, though, and this one runs on whatever the
//! CI provider hands it, so the ceiling is scaled by a calibration loop
//! that touches nothing in zu. See `calibrate`.
//!
//! Run: ZU_GATE=1 cargo bench -p zu --bench refuse

use std::time::Instant;

use zu::gqlstatus::codes;
use zu::query::Value;
use zu::session::Session;
use zu::zu1::file::Zu1File;
use zu::zu1::graph::bulk_load_as;
use zu::{GqlStatus, ZuError};

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

/// How fast this host is at the kind of work a refusal is, in
/// nanoseconds per round, measured without touching the engine at all.
///
/// A refusal is a short string walked into a few small allocations and
/// a lookup or two, and it is over before it touches any data. So the
/// loop below is a short string walked into a small allocation and a
/// lookup: it interns a word into a map, reads two back out and formats
/// a number, which is the parser, the symbol table and the message in
/// the shape they cost. A pointer chase was tried first and is not the
/// right proxy, because the runner has memory as fast as the reference
/// machine and a core that is not, so the chase reads 0.99x on a host
/// that runs the refusal at 1.7x.
fn calibrate() -> f64 {
    const ROUNDS: usize = 20_000;
    const WORDS: usize = 64;
    let words: Vec<String> = (0..WORDS).map(|i| format!("identifier_{i:03}")).collect();
    let mut rng = 0x2064u64;
    let mut sink = 0usize;
    let round = |rng: &mut u64, sink: &mut usize| {
        let mut seen: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
        for _ in 0..8 {
            let w = &words[(xorshift(rng) % WORDS as u64) as usize];
            let n = seen.len();
            seen.entry(w.clone()).or_insert(n);
        }
        for _ in 0..8 {
            let w = &words[(xorshift(rng) % WORDS as u64) as usize];
            *sink += seen.get(w.as_str()).copied().unwrap_or(0);
        }
        let msg = format!(
            "undefined reference to {} at {}",
            words[*sink % WORDS],
            *sink
        );
        *sink += msg.len();
    };
    for _ in 0..ROUNDS / 10 {
        round(&mut rng, &mut sink);
    }
    let start = Instant::now();
    for _ in 0..ROUNDS {
        round(&mut rng, &mut sink);
    }
    let ns = start.elapsed().as_nanos() as f64 / ROUNDS as f64;
    std::hint::black_box(sink);
    ns
}

const NODES: u32 = 10_000;
const ANSWER_Q: &str = "MATCH (a:person {id: $src})-[:follows]->(b) RETURN count(b) AS n";

/// The statements this bench asks the engine to refuse, with the stage
/// each one dies at and the condition it has to raise.
const REFUSALS: &[(&str, &str, GqlStatus)] = &[
    (
        "syntax",
        "MATCH (a:person)-[:follows]->( RETURN count(a) AS n",
        codes::C42001,
    ),
    (
        "undefined reference",
        "MATCH (a:person)-[:follows]->(b) RETURN c.id AS n",
        codes::C42002,
    ),
    ("missing parameter", ANSWER_Q, codes::C42002),
];

/// Builds the synthetic graph and returns out-degree by node, the
/// reference the answer path checks against.
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

/// Median latency of the warm point read, every answer checked against
/// the degree table. This is the number the refusals are held against.
fn run_answer(path: &std::path::Path, degree: &[i64]) -> f64 {
    let mut session = Session::open(path).expect("open");
    let mut rng = 0xbeefu64;
    for _ in 0..100 {
        let src = (xorshift(&mut rng) % u64::from(NODES)) as i64;
        session
            .run(ANSWER_Q, &[("src", Value::Int(src))])
            .expect("warmup");
    }
    let mut lat: Vec<u64> = Vec::with_capacity(10_000);
    for _ in 0..10_000 {
        let src = (xorshift(&mut rng) % u64::from(NODES)) as i64;
        let start = Instant::now();
        let r = session
            .run(ANSWER_Q, &[("src", Value::Int(src))])
            .expect("point");
        lat.push(start.elapsed().as_nanos() as u64);
        assert_eq!(count_of(&r), degree[src as usize], "src {src}");
    }
    p50(lat)
}

/// Median latency of one refusal, asserting the condition every time.
///
/// The session is warmed the way a client that keeps sending the same
/// bad statement would warm it, which is the shape that matters: if
/// there is a cache on this path it is the engine's to use, and if
/// there is not, the parse it repeats is the honest cost.
fn run_refusal(
    path: &std::path::Path,
    source: &str,
    params: &[(&str, Value)],
    want: GqlStatus,
) -> f64 {
    let mut session = Session::open(path).expect("open");
    for _ in 0..100 {
        refuse(&mut session, source, params, want);
    }
    let mut lat: Vec<u64> = Vec::with_capacity(10_000);
    for _ in 0..10_000 {
        let start = Instant::now();
        let err = session.run(source, params).err();
        lat.push(start.elapsed().as_nanos() as u64);
        check(err, want, source);
    }
    p50(lat)
}

fn refuse(session: &mut Session, source: &str, params: &[(&str, Value)], want: GqlStatus) {
    check(session.run(source, params).err(), want, source);
}

fn check(err: Option<ZuError>, want: GqlStatus, source: &str) {
    let Some(err) = err else {
        panic!("the engine answered a statement this bench needs it to refuse: {source}");
    };
    assert_eq!(
        err.gqlstatus(),
        Some(want),
        "wrong condition for {source}: {err}"
    );
}

fn main() {
    let gate = std::env::var("ZU_GATE").is_ok_and(|v| v == "1");
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("refuse.zu1");
    let degree = build(&path);
    let edge_count: i64 = degree.iter().sum();
    println!("refuse bench graph: {NODES} nodes, {edge_count} edges");

    let answer_us = run_answer(&path, &degree);
    println!("answer path (warm point read): p50 {answer_us:.2} us over 10000 reads");

    let mut worst = ("", 0.0f64);
    for (name, source, want) in REFUSALS {
        let params: &[(&str, Value)] = if *name == "missing parameter" {
            &[]
        } else {
            &[("src", Value::Int(1))]
        };
        let us = run_refusal(&path, source, params, *want);
        println!(
            "refusal, {name} ({code}): p50 {us:.2} us, {ratio:.2}x the answer path",
            code = want,
            ratio = us / answer_us.max(0.001)
        );
        if us > worst.1 {
            worst = (name, us);
        }
    }

    // Printed, not gated: this one is raised with rows in flight, so it
    // is an answer plus a check by construction. It is a keyed point
    // read with a division on the projection, so the comparison is
    // against the same kind of work rather than against a scan, and it
    // is here because a runtime refusal costing many times an answer
    // would mean the check itself is expensive.
    let divide_us = run_refusal(
        &path,
        "MATCH (a:person {id: $src}) RETURN a.id / 0 AS n",
        &[("src", Value::Int(1))],
        codes::C22012,
    );
    println!(
        "runtime refusal, divide by zero (22012): p50 {divide_us:.2} us, \
         {ratio:.2}x the answer path (information, not gated)",
        ratio = divide_us / answer_us.max(0.001)
    );

    let ratio = worst.1 / answer_us.max(0.001);
    println!(
        "slowest gated refusal: {name} at p50 {us:.2} us, {ratio:.2}x the answer path",
        name = worst.0,
        us = worst.1
    );

    // The scale the absolute ceiling is read at. Clamped below at 1, so
    // a fast host is held to the written number and never to a tighter
    // one, and above at 4, so a host slow enough to make the backstop
    // meaningless says so rather than passing quietly.
    let cal_ns = calibrate();
    let reference = budget("refuse_calibration_ns").unwrap_or(cal_ns);
    let raw = cal_ns / reference.max(0.001);
    let scale = raw.clamp(1.0, 4.0);
    println!(
        "host calibration: {cal_ns:.0} ns per round, {raw:.2}x the reference {reference:.0} ns"
    );
    if raw > scale {
        println!(
            "host calibration: {raw:.2}x is past the 4x cap, the latency ceiling is read at 4x"
        );
    }

    let mut failed = false;
    if let Some(ceiling) = budget("refuse_over_answer")
        && ratio > ceiling
    {
        println!(
            "GATE FAIL refusal against answer: {name} at {ratio:.2}x > ceiling {ceiling}",
            name = worst.0
        );
        failed = true;
    }
    if let Some(ceiling) = budget("refuse_p50_us") {
        let scaled = ceiling * scale;
        println!("latency ceiling: {ceiling:.2} us written, {scaled:.2} us on this host");
        if worst.1 > scaled {
            println!(
                "GATE FAIL refusal latency: {name} at p50 {us:.2} us > ceiling {scaled:.2}",
                name = worst.0,
                us = worst.1
            );
            failed = true;
        }
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
