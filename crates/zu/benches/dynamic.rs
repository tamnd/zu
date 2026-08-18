//! G1 dynamic value path, measured on its own (Spec/2064g/gql/plan/10
//! section 5, zu#116).
//!
//! The risk the type system carries is that fifty two types come back
//! as a tag on every cell, which is the thing the executor rewrite
//! exists to remove. The mitigation the plan names has two halves: a
//! comparison whose types the compiler knows runs a kernel, which
//! `compare.rs` gates, and everything left over is measured separately
//! and stays off every other benchmark's critical path. This is the
//! second half.
//!
//! Five filters run over the same two million people. The first is a
//! bound the compiler types, the shape the kernel is for, and the four
//! after it are the same bound written with a cast or a type predicate
//! around it. They come in pairs, and the pair is the point.
//!
//! A cast to INT64 over a column already stored as a 64 bit integer is
//! the value it already was, so the compiler drops it and the filter
//! stays on the kernel. A cast to INT8 is a range check per row, which
//! no kernel has an op for, so the whole query goes back to the row
//! engine where each value carries its tag and each row is walked one
//! at a time. A type predicate splits the same way: asking a column
//! whether it is an INT64 is a question its type answers before a row
//! is read, and asking whether it is an INT8 is a question about each
//! number. The answers are crosschecked against the generator on every
//! run, so a filter that stopped selecting the same people fails here
//! rather than printing a number.
//!
//! What the ratios say is where the line between the two paths falls
//! today, and what a query pays for landing on the wrong side of it.
//! Nothing in the rest of the bench suite writes a cast or a type
//! predicate, which is the other thing this file is here to keep true:
//! a grep for CAST or IS TYPED across `crates/zu/benches` finds this
//! file and no other.
//!
//! The reference values of GV60 and GV61 are last, and they are not a
//! per-row path at all. A statement carries a handle as a parameter, so
//! the work is one catalog lookup or one epoch compare for the whole
//! statement, and what is measured is a warm point read with a graph
//! handle bound beside its key against the same read without one.
//!
//! There is no budget key here on purpose. The floor that matters is
//! the kernel bound's, and it is gated in `compare.rs`; a ceiling on
//! the row engine would be a promise about a path this milestone is
//! explicitly not making fast.
//!
//! Run: cargo bench -p zu --bench dynamic

use std::time::Instant;

use zu::query::{self, Value};
use zu::session::Session;
use zu::zu1::file::Zu1File;
use zu::zu1::graph::bulk_load_as;
use zu::zu1::props::{PropValues, store_props};

const NODES: u64 = 2_000_000;

/// Strided ages, so every chunk holds the whole range and no zone map
/// rules one out. What a filter costs here is the filter.
fn age_of(i: u64) -> i64 {
    ((i * 7919) % 100) as i64
}

fn build(path: &std::path::Path) {
    let mut db = Zu1File::create(path).expect("create");
    bulk_load_as(&mut db, "person", "knows", NODES, &[]).expect("load");
    let age: Vec<u64> = (0..NODES).map(|i| age_of(i) as u64).collect();
    store_props(&mut db, "person", &[("age", PropValues::Int(&age))]).expect("props");
}

fn count(r: &zu::query::QueryResult) -> i64 {
    assert_eq!(r.rows.len(), 1, "a counting query returns one row");
    match r.rows[0][0] {
        Value::Int(n) => n,
        ref other => panic!("expected a count, got {other:?}"),
    }
}

/// Median ms of `source`, with the count checked on every run.
fn measure(db: &mut Zu1File, source: &str, want: i64, runs: usize) -> f64 {
    let warm = query::run(source, db, &[]).expect("warmup");
    assert_eq!(count(&warm), want, "warmup answer for {source}");
    let mut times: Vec<f64> = (0..runs)
        .map(|_| {
            let t = Instant::now();
            let r = query::run(source, db, &[]).expect("timed run");
            let ms = t.elapsed().as_secs_f64() * 1e3;
            assert_eq!(count(&r), want, "answer changed for {source}");
            ms
        })
        .collect();
    times.sort_by(f64::total_cmp);
    times[times.len() / 2]
}

/// Median microseconds of a warm point read, with `params` bound.
fn point(session: &mut Session, source: &str, params: &[(&str, Value)], runs: usize) -> f64 {
    session.run(source, params).expect("warmup");
    let mut times: Vec<f64> = (0..runs)
        .map(|_| {
            let t = Instant::now();
            session.run(source, params).expect("timed run");
            t.elapsed().as_secs_f64() * 1e6
        })
        .collect();
    times.sort_by(f64::total_cmp);
    times[times.len() / 2]
}

fn main() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("dynamic.zu1");
    let t = Instant::now();
    build(&path);
    let mut db = Zu1File::open(&path).expect("open");
    // SAFETY: the bench main is single threaded here; workers only
    // spawn inside the timed calls below.
    unsafe { std::env::set_var("ZU_THREADS", "1") };
    println!(
        "dynamic: {NODES} persons, load {:.1}s",
        t.elapsed().as_secs_f64()
    );

    let over_30 = (0..NODES).filter(|&i| age_of(i) > 30).count() as i64;
    // Every age is under a hundred, so the checked forms below check
    // and pass. What they cost is the checking and not an error path.
    let cases: [(&str, &str, i64); 5] = [
        (
            "kernel bound",
            "MATCH (p:person) WHERE p.age > 30 RETURN count(p) AS n",
            over_30,
        ),
        (
            "cast, dropped",
            "MATCH (p:person) WHERE CAST(p.age AS INT64) > 30 RETURN count(p) AS n",
            over_30,
        ),
        (
            "cast, checked",
            "MATCH (p:person) WHERE CAST(p.age AS INT8) > 30 RETURN count(p) AS n",
            over_30,
        ),
        (
            "type predicate, settled",
            "MATCH (p:person) WHERE p.age IS TYPED INT64 RETURN count(p) AS n",
            NODES as i64,
        ),
        (
            "type predicate, per row",
            "MATCH (p:person) WHERE p.age IS TYPED INT8 RETURN count(p) AS n",
            NODES as i64,
        ),
    ];

    let mut kernel = 0.0;
    for (what, source, want) in cases {
        let ms = measure(&mut db, source, want, 7);
        let mrows = NODES as f64 / 1e3 / ms;
        if what == "kernel bound" {
            kernel = ms;
            println!("dynamic {what}: {ms:.1} ms {mrows:.0} M rows/s, crosschecked");
        } else {
            println!(
                "dynamic {what}: {ms:.1} ms {mrows:.0} M rows/s, {:.1}x the kernel bound, \
                 crosschecked",
                ms / kernel
            );
        }
    }

    drop(db);
    let mut session = Session::open(&path).expect("open");
    let handle = session.graph_ref("/", "home").expect("the home graph");
    // The cheapest statement there is, so what the two numbers differ
    // by is the parameter check and not the query under it.
    let source = "RETURN $a AS n";
    let bare = point(&mut session, source, &[("a", Value::Int(30))], 20_000);
    let held = point(
        &mut session,
        source,
        &[("a", Value::Int(30)), ("g", handle)],
        20_000,
    );
    println!(
        "dynamic reference parameter: {bare:.2} us bare, {held:.2} us with a graph handle bound, \
         {:+.2} us per statement",
        held - bare
    );
}
