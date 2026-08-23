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
//! CI provider hands it. That ceiling is held on a host that reads
//! close to the machine it was written for and is reported and not held
//! anywhere else. See [`REFERENCE_ANSWER_US`].
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

/// What the answer path costs on the machine the absolute ceiling was
/// written for, in microseconds. A laptop M4 with its cores to itself.
///
/// Three runs on an idle one read 3.71, 3.62 and 3.67, so the spread is
/// two percent and [`ENFORCE_WITHIN`] covers it several times over.
/// Take it on an idle machine or not at all: the same reading during a
/// background `cargo test` was 7.46.
///
/// It was 5.50 until #655 took a parse off every send, which is worth
/// saying here because the number moving is the engine getting faster
/// and not the host changing.
///
/// This is not a number to tune. It is a property of one host, and the
/// only reason to change it is that the host has been replaced.
const REFERENCE_ANSWER_US: f64 = 3.67;

/// How far past the reference a host may read and still be held to the
/// written microsecond ceiling.
///
/// A quarter, which is several times the spread of a quiet machine and
/// nowhere near the two times a hosted runner reads. The point is to
/// separate the same class of machine from a different one, not to
/// grade them.
const ENFORCE_WITHIN: f64 = 1.25;

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

    // Which class of machine this is, on zu's own work rather than on a
    // proxy for it. Four proxies were tried and none of them tracked: a
    // pointer chase read 0.99x, a string and map loop in the shape a
    // refusal costs read 1.15x on one runner and 1.61x on the next, and
    // the store build reads under one because it is a filesystem
    // measurement and Linux takes unsynced writes more cheaply than
    // Darwin does. The answer path is the one thing in this bench that
    // is zu, is timed the same way on every host, and moves with the
    // machine: the two runners that read 1.15x and 1.61x on the loop
    // read 1.94x and 2.33x here. See #648.
    let host = answer_us / REFERENCE_ANSWER_US;
    println!(
        "host: answer path {answer_us:.2} us, {host:.2}x the reference \
         {REFERENCE_ANSWER_US:.2} us"
    );

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
        if host > ENFORCE_WITHIN {
            // A microsecond ceiling is a statement about a machine, and
            // this one is not that machine. Scaling the ceiling by how
            // much slower it is was the old answer and it was wrong
            // twice over: no proxy tracked, and the one thing that does
            // track is the path the ratio above already divides by, so
            // scaling by it turns the backstop into refuse_over_answer
            // with a looser constant. So it is reported here and held
            // where it means something.
            println!(
                "latency ceiling: {ceiling:.2} us written, not held on this host at {host:.2}x \
                 the reference. The slowest refusal read p50 {us:.2} us. The ratio gate above \
                 runs everywhere and is the one this host enforces.",
                us = worst.1
            );
        } else {
            println!("latency ceiling: {ceiling:.2} us written, held on this host at {host:.2}x");
            if worst.1 > ceiling {
                println!(
                    "GATE FAIL refusal latency: {name} at p50 {us:.2} us > ceiling {ceiling:.2}",
                    name = worst.0,
                    us = worst.1
                );
                failed = true;
            }
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
